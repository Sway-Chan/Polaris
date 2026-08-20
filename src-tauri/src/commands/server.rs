//! 节点（server）类 command（上游 `server-handlers.ts`）。
//!
//! 映射 channel：
//! - `server:add` → [`server_add`]
//! - `server:addBulk` → [`server_add_bulk`]
//! - `server:update` → [`server_update`]
//! - `server:delete` → [`server_delete`]
//! - `server:deleteBatch` → [`server_delete_batch`]
//! - `server:getAll` → [`server_get_all`]
//! - `server:switch` → [`server_switch`]
//! - `server:generateUrl` → [`server_generate_url`]（config-engine ProtocolParser 等价）
//! - `warp:register` / `warp:applyLicense` → [`warp_register`] / [`warp_apply_license`]（mesh crate）
//! - `tailscale:login` / `loginCancel` / `logout` / `stateExists` / `getStatus` → tailscale_* （mesh crate）
//!
//! 节点 CRUD 经 config 的 load/save（servers 数组原地改 + 原子写）+ 广播 event:configChanged。
//! DIRECT_SERVER_ID 哨兵 + 删选中节点的兜底出口逻辑对齐 Polaris（D4/F-1）。

#![allow(clippy::needless_pass_by_value)]

use serde_json::{json, Map, Value};
use tauri::{AppHandle, State};

use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::server_config::ServerConfig;
use polaris_mesh::warp_http::RegisterOptions;

use crate::commands::config::broadcast_config_changed;
use crate::response::{ok_void, ApiResponse};
use crate::runtime::config::{ConfigManager, Decision};
use crate::runtime::tailscale_login_core::StartLoginOutcome;
use crate::runtime::unlock::{selected_exit_changed, BroadcastSink};
use crate::runtime::AppRuntime;

/// Polaris 直接选择哨兵（`shared/direct-selection.ts DIRECT_SERVER_ID`）。
const DIRECT_SERVER_ID: &str = "__direct__";

/// id 缺失 / 空 → mint uuid（镜像 [`server_add_bulk`] 的 `s["id"]=new_uuid()`）。
///
/// 此前 `server_add` 直接 push 原值不补 id → `store::sanitize` 丢弃 id 缺失/空的节点（要求 id 非空字符串）
/// → 克隆 / 手动加的节点产出不可用、不持久。非对象入参不动（交 sanitize 丢弃）。
fn ensure_server_id(mut server: Value) -> Value {
    if let Some(obj) = server.as_object_mut() {
        let has_id = obj
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !has_id {
            obj.insert("id".to_string(), json!(new_uuid()));
        }
    }
    server
}

/// `server:add` 核心（注入 `ConfigManager`，便于真实 ConfigStore 驱动测试）：补 id + 落盘，返回新 config。
fn server_add_core(config: &ConfigManager, server: Value) -> Result<Value, String> {
    let mut cfg = config.load_full().map_err(|e| format!("{e}"))?;
    if let Some(servers) = cfg.get_mut("servers").and_then(Value::as_array_mut) {
        servers.push(ensure_server_id(server));
    }
    config.save_full(&cfg).map_err(|e| format!("{e}"))?;
    Ok(cfg)
}

/// 上游 `SERVER_ADD`：新增节点（id 缺失/空则 mint，防 sanitize 丢弃）。
///
/// DESIGN-REVIEW(mesh-singleton-guard-renderer-only)：**WARP / Tailscale 单例槽的闸门只在渲染端**
/// （`ui/src/domain/endpoint-routes.ts#meshSingletonConflict`，接线于 NodeDialog / WgDialog /
/// ImportDialog / 节点克隆四条腿；WarpDialog 由接入区卡片分流、TsLoginDialog 由
/// `planTsLoginSubmit` 复用既有节点，结构上不产生第二实例）。本命令与 [`server_add_bulk`] 刻意**不加**
/// 对应守卫，理由：
///  1. 判定谓词 `isWarpServer` 在 Rust 侧无对应物（`domain/warp.ts` 头注已登记此边界：输入均在前端
///     store，漂移后果止于 UI）。在此复刻一份「端点域名兜底 + warpDevice 标记」的启发式 = 造第二真值源，
///     无 codegen 约束，日后必然与 TS 侧分叉——这正是本仓反复吃过的亏。
///  2. 误判方向不可接受：本命令同时是备份恢复 / 导入的落盘substrate，Rust 侧启发式误拒一个合法节点，
///     用户在 UI 上无从修复；而渲染端误拒最多是弹一次错、用户改地址重来。
///  3. 威胁模型：`server:add` 只被本应用自己的 webview 调用，不接受外部不可信输入。
///
/// 若日后新增**非渲染端**的写入方（CLI / 深链接 / 远程配置下发），本决定即失效，须在此补守卫。
#[tauri::command]
pub fn server_add(app: AppHandle, state: State<'_, AppRuntime>, server: Value) -> ApiResponse<()> {
    match server_add_core(state.config(), server) {
        Ok(cfg) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(e),
    }
}

/// `server:addBulk` 核心（注入 `ConfigManager`，便于真实 ConfigStore 驱动测试）：强制重生成 id +
/// 剥离订阅归属 + 补时间戳，落盘，返回 `(新 config, 实际新增数)`。
///
/// `added` 须在**消费 `servers` 之前**快照——下方两分支（`for` / `init` move）都会吃掉 `servers`；
/// 对齐 上游 `SERVER_ADD_BULK`（`server-handlers.ts`：`added = list.length`，入参全部入库、无逐项过滤）。
/// 此前命令层硬编码 `"added":0` → 契约破（消费方若按 added 判成功恒见 0）。
fn server_add_bulk_core(
    config: &ConfigManager,
    servers: Vec<Value>,
) -> Result<(Value, usize), String> {
    let added = servers.len();
    let mut cfg = config.load_full().map_err(|e| format!("{e}"))?;
    let now = current_iso();
    if let Some(arr) = cfg.get_mut("servers").and_then(Value::as_array_mut) {
        for mut s in servers {
            // 强制重生成 id（防撞）+ 剥离订阅归属（自建节点恒可编辑可删除）。
            s["id"] = json!(new_uuid());
            if let Some(obj) = s.as_object_mut() {
                obj.remove("subscriptionId");
                obj.remove("providerName");
            }
            s["createdAt"] = s.get("createdAt").cloned().unwrap_or_else(|| json!(now));
            s["updatedAt"] = json!(now);
            arr.push(s);
        }
    } else {
        // servers 字段缺失/非数组 → 用 servers 入参（已补全 id/时间）初始化。
        let mut init: Vec<Value> = servers;
        for s in &mut init {
            s["id"] = json!(new_uuid());
            s["updatedAt"] = json!(now);
        }
        if let Some(obj) = cfg.as_object_mut() {
            obj.insert("servers".to_string(), Value::Array(init));
        }
    }
    config.save_full(&cfg).map_err(|e| format!("{e}"))?;
    Ok((cfg, added))
}

/// 上游 `SERVER_ADD_BULK`：批量新增自建节点（强制重生成 id + 剥离 subscriptionId/providerName）。
#[tauri::command]
pub fn server_add_bulk(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    servers: Vec<Value>,
) -> ApiResponse<Value> {
    if servers.is_empty() {
        return ApiResponse::ok(json!({ "added": 0 }));
    }
    match server_add_bulk_core(state.config(), servers) {
        Ok((cfg, added)) => {
            broadcast_config_changed(&app, &cfg);
            ApiResponse::ok(json!({ "added": added }))
        }
        Err(e) => ApiResponse::err(e),
    }
}

/// 上游 `SERVER_UPDATE`：更新节点（按 id）。
#[tauri::command]
pub fn server_update(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    server: Value,
) -> ApiResponse<()> {
    let id = server.get("id").and_then(Value::as_str).map(str::to_string);
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let id = match id {
        Some(i) => i,
        None => return ApiResponse::err("server.id required"),
    };
    let found = cfg
        .get_mut("servers")
        .and_then(Value::as_array_mut)
        .map(|arr| {
            if let Some(idx) = arr
                .iter()
                .position(|s| s.get("id").and_then(Value::as_str) == Some(&id))
            {
                arr[idx] = server;
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if !found {
        return ApiResponse::err(format!("服务器不存在: {id}"));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 删选中节点后的新 `selectedServerId`（D4）：兜底出口须**存活于删除后的 `cfg.servers`** 才采用，否则回落直连
/// 哨兵（**不可置 null**：0 节点重启会 throw）。
///
/// 单删 / 批删共用同一份 viable 校验——此前批删压根没有 `fallback_selected_id` 形参、恒落直连，与单删口径分叉。
fn resolve_fallback_selected(cfg: &Value, fallback_selected_id: Option<&str>) -> String {
    let viable = fallback_selected_id.is_some_and(|fb| {
        cfg.get("servers")
            .and_then(Value::as_array)
            .is_some_and(|arr| {
                arr.iter()
                    .any(|s| s.get("id").and_then(Value::as_str) == Some(fb))
            })
    });
    match fallback_selected_id {
        Some(fb) if viable => fb.to_string(),
        _ => DIRECT_SERVER_ID.to_string(),
    }
}

/// 删节点后更新选中出口（单删 / 批删共用的纯 `Value` 变换）：选中节点落在删除集内（`selected_removed`）→
/// 兜底出口（存活候选或直连哨兵），否则不动。`cfg` 须为**已剔除被删节点后**的配置（viable 校验看删后 servers）。
///
/// A7：出口只可能从被删的旧 id 变为 viable 兜底 / `__direct__` 哨兵（**恒不等于被删 id**）——故 `selected_removed`
/// 为真即出口变；命令层落盘后再用 `selected_exit_changed(old, new)` 统一判定失效（此腿走哨兵，不产生 →null）。
fn apply_selection_fallback(
    cfg: &mut Value,
    selected_removed: bool,
    fallback_selected_id: Option<&str>,
) {
    if selected_removed {
        let next = resolve_fallback_selected(cfg, fallback_selected_id);
        if let Some(obj) = cfg.as_object_mut() {
            obj.insert("selectedServerId".to_string(), json!(next));
        }
    }
}

/// 上游 `SERVER_DELETE`：删除单节点（删选中 → 兜底出口）+ tailscale/warp 副作用。
#[tauri::command]
pub fn server_delete(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    server_id: String,
    fallback_selected_id: Option<String>,
) -> ApiResponse<()> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    // A7：落盘前快照旧选中出口（删当前选中 → 回落兜底出口 = 出口变，作废旧出口解锁缓存）。
    let old_selected = cfg
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let removed = {
        let arr = cfg.get_mut("servers").and_then(Value::as_array_mut);
        arr.and_then(|arr| {
            arr.iter()
                .position(|s| s.get("id").and_then(Value::as_str) == Some(&server_id))
                .map(|idx| arr.remove(idx))
        })
    };
    let Some(removed) = removed else {
        return ApiResponse::err(format!("服务器不存在: {server_id}"));
    };
    // 删选中 → 兜底出口（防 0 节点重启 throw；不可置 null）。
    let was_selected = old_selected.as_deref() == Some(&server_id);
    apply_selection_fallback(&mut cfg, was_selected, fallback_selected_id.as_deref());
    // 被删节点的 id 若留在 MRU 里会永久占住一个槽位（见 `prune_recent_server_ids` 文档）。
    prune_recent_server_ids(&mut cfg, |id| id == server_id);
    match state.config().save_full(&cfg) {
        Ok(()) => {
            // 删除副作用：tailscale state 清理（best-effort，不阻断删除）。
            run_server_removal_side_effects(&state, &removed);
            broadcast_config_changed(&app, &cfg);
            // A7：删当前选中 → selectedServerId 回落 viable/`__direct__`（恒 != 旧 id）= 出口变 →
            // 作废旧出口解锁探测缓存（否则解锁角标最长陈旧 30min）。删非选中节点 → 出口不动、不失效。
            if selected_exit_changed(
                old_selected.as_deref(),
                cfg.get("selectedServerId").and_then(Value::as_str),
            ) {
                let sink = BroadcastSink::new(&app);
                let running = state.proxy().status().running;
                state.unlock().invalidate(&sink, running, false);
            }
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `SERVER_DELETE_BATCH`：批量删除（返回实际删除数）。
///
/// `fallback_selected_id`：选中节点落在删除集内时的兜底出口（渲染端按「剩余节点里最快」算，见 `pickFallbackExit`）。
/// 此前**无此形参** → 前端传的 key 被 Tauri 静默丢弃、批删掉当前出口恒落直连（流量裸奔）；现与单删同口径。
#[tauri::command]
pub fn server_delete_batch(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    server_ids: Vec<String>,
    fallback_selected_id: Option<String>,
) -> ApiResponse<u32> {
    let id_set: std::collections::HashSet<&str> = server_ids.iter().map(String::as_str).collect();
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    // A7：落盘前快照旧选中出口（选中落删除集 → 回落兜底出口 = 出口变，作废旧出口解锁缓存）。
    let old_selected = cfg
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let removed: Vec<Value> = {
        let arr = cfg.get_mut("servers").and_then(Value::as_array_mut);
        match arr {
            Some(arr) => {
                let (keep, drop) = arr.drain(..).partition(|s| {
                    !id_set.contains(s.get("id").and_then(Value::as_str).unwrap_or(""))
                });
                *arr = keep;
                drop
            }
            None => Vec::new(),
        }
    };
    if removed.is_empty() {
        return ApiResponse::ok(0u32);
    }
    // 选中在删除集内 → 兜底出口。
    let selected_in_set = old_selected
        .as_deref()
        .map(|sid| id_set.contains(sid))
        .unwrap_or(false);
    apply_selection_fallback(&mut cfg, selected_in_set, fallback_selected_id.as_deref());
    // 同单删：被删节点的 id 不得留在 MRU 里占槽位。
    prune_recent_server_ids(&mut cfg, |id| id_set.contains(id));
    match state.config().save_full(&cfg) {
        Ok(()) => {
            for r in &removed {
                run_server_removal_side_effects(&state, r);
            }
            broadcast_config_changed(&app, &cfg);
            // A7：选中落删除集 → selectedServerId 回落 viable/`__direct__`（恒 != 旧 id）= 出口变 →
            // 作废旧出口解锁探测缓存。选中不在删除集 → 出口不动、不失效。
            if selected_exit_changed(
                old_selected.as_deref(),
                cfg.get("selectedServerId").and_then(Value::as_str),
            ) {
                let sink = BroadcastSink::new(&app);
                let running = state.proxy().status().running;
                state.unlock().invalidate(&sink, running, false);
            }
            ApiResponse::ok(u32::try_from(removed.len()).unwrap_or(0))
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `SERVER_GET_ALL`：取全部节点。
#[tauri::command]
pub fn server_get_all(state: State<'_, AppRuntime>) -> ApiResponse<Vec<Value>> {
    match state.config().current() {
        Ok(cfg) => {
            let empty = Vec::new();
            let servers = cfg
                .get("servers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or(empty);
            ApiResponse::ok(servers)
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 最近连接节点入队（托盘「节点·最近」MRU，原型 `pickNode`：
/// `st.mru = [name, ...st.mru.filter(x=>x!==name)].slice(0,3)` 同款语义）：去重后插入队首，上限 3
/// （与原型的 `.slice(0,3)` 对齐，前端再叠加「当前节点置顶」渲染，见 TrayMenu.tsx recentItems）。
///
/// 只在真实节点切换（[`server_switch`]）时调用；直连哨兵切换走 `config_save` 全量保存，不经此路径 ——
/// 与原型 `pickDirectExit` 不碰 `st.mru` 对齐。存量配置无 `recentServerIds` 字段 → 视作空历史，非结构错误。
fn push_recent_server_id(obj: &mut Map<String, Value>, server_id: &str) {
    let mut recent: Vec<String> = obj
        .get("recentServerIds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    recent.retain(|id| id != server_id);
    recent.insert(0, server_id.to_string());
    recent.truncate(3);
    obj.insert("recentServerIds".to_string(), json!(recent));
}

/// 从托盘 MRU 历史剔除已删除节点的 id（单删 / 批删共用的纯 `Value` 变换）。
///
/// # 为什么必须剔
///
/// `TrayMenu` 渲染「节点·最近」时按 id 反查 `ServerConfig`，**查不到即跳过且不回填**
/// （`ids.map(id => servers.find(...)).filter(Boolean)`）⇒ 一个指向已删节点的死 id 会永久占住
/// `truncate(3)` 的三个槽位之一，表现为「最近节点恒少显示一条」。删除节点是这份 MRU 的所有者
/// （后端）唯一能观察到「该 id 已失效」的时机 —— 前端对该字段无写入权，剔不了也不该剔。
///
/// 无 `recentServerIds` 键（存量配置）→ 空操作，不凭空建键。剔后为空 → 落空数组（不删键，
/// 保持形状稳定；空数组与缺键对读侧 `?? []` 等价）。
fn prune_recent_server_ids(cfg: &mut Value, is_removed: impl Fn(&str) -> bool) {
    let Some(obj) = cfg.as_object_mut() else {
        return;
    };
    let Some(arr) = obj.get_mut("recentServerIds").and_then(Value::as_array_mut) else {
        return;
    };
    arr.retain(|v| v.as_str().is_some_and(|id| !is_removed(id)));
}

/// `server:switch` 的原子读改写核心。快速连点会并发到达 command 线程；必须复用
/// [`ConfigManager::update`] 把「验证节点 → 取旧出口 → 改选中/MRU → 落盘」圈成一个动作，
/// 否则两个请求都基于同一份旧配置写回，较慢者会覆盖较新的选择与 MRU。
fn server_switch_core(config: &ConfigManager, server_id: &str) -> Result<(Value, bool), String> {
    let (exit_changed, saved) = config
        .update(|cfg| {
            let exists = cfg
                .get("servers")
                .and_then(Value::as_array)
                .is_some_and(|arr| {
                    arr.iter()
                        .any(|s| s.get("id").and_then(Value::as_str) == Some(server_id))
                });
            if !exists {
                return Decision::Skip(Err(format!("服务器不存在: {server_id}")));
            }
            // A7：出口节点 identity 是否真变（用于切换后作废旧出口的解锁缓存）。取覆盖前的旧值比对。
            let exit_changed = selected_exit_changed(
                cfg.get("selectedServerId").and_then(Value::as_str),
                Some(server_id),
            );
            if let Some(obj) = cfg.as_object_mut() {
                obj.insert("selectedServerId".to_string(), json!(server_id));
                push_recent_server_id(obj, server_id);
            }
            Decision::Write(Ok(exit_changed))
        })
        .map_err(|e| format!("{e}"))?;
    let exit_changed = exit_changed?;
    let cfg = saved.expect("server_switch 的 Write 腿必须返回已落盘配置");
    Ok((cfg, exit_changed))
}

/// 上游 `SERVER_SWITCH`：切换选中节点。
#[tauri::command]
pub fn server_switch(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    server_id: String,
) -> ApiResponse<()> {
    match server_switch_core(state.config(), &server_id) {
        Ok((cfg, exit_changed)) => {
            broadcast_config_changed(&app, &cfg);
            // A7：换节点 = 出口 identity 变 → 作废旧出口的解锁探测缓存（否则解锁角标最长陈旧 30min，
            // 即缓存 FRESH_TTL）。重选同一节点（identity 未变）不失效，避免白刷探测。
            // exit_blocked=false：切换瞬间尚未探新出口，交前端按 running 复位「检测中」并重跑（对齐 invalidate 契约）。
            if exit_changed {
                let sink = BroadcastSink::new(&app);
                let running = state.proxy().status().running;
                state.unlock().invalidate(&sink, running, false);
            }
            ok_void()
        }
        Err(e) => ApiResponse::err(e),
    }
}

/// 上游 `SERVER_GENERATE_URL`：节点 → 真实 share URL（`ProtocolParser.generateUrl` 等价）。
///
/// 反序列化 `ServerConfig` 后走 net-stack [`encode_share_url`](polaris_net_stack::share_link::encode_share_url)
/// —— 即解析器 `parse_share_url` 的**逆**（`vless://…`/`vmess://base64(json)`/`ss://…`/`trojan://…` 等，
/// round-trip 金样测试逐协议锁死）。此前返回的 `polaris://name/<uuid>` 是**假链**（无法被任何客户端导入，
/// 也无法 round-trip）——已替换为真实协议 URI。
///
/// 结构非法（缺字段/端口越界）或**无标准分享链接形态的协议**（WireGuard/Tailscale/SSH/Custom）→ err 信封，
/// 前端据此提示，不吐假链。
#[tauri::command]
pub fn server_generate_url(_state: State<'_, AppRuntime>, server: Value) -> ApiResponse<String> {
    let cfg: ServerConfig = match serde_json::from_value(server) {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("节点结构非法: {e}")),
    };
    match polaris_net_stack::share_link::encode_share_url(&cfg) {
        Ok(url) => ApiResponse::ok(url),
        Err(e) => ApiResponse::err(e),
    }
}

/// 上游 `WARP_REGISTER`：注册匿名 WARP 设备 → WireGuard 草稿（mesh crate WarpService）。
///
/// 装配见 `runtime::mesh::warp_service`：注入 [`HttpRuntime`](crate::runtime::http::HttpRuntime) 的
/// `WarpHttp`（reqwest+rustls）+ ring CSPRNG 种子 + RFC 7748 X25519 公钥（`runtime::x25519`）。
///
/// ⚠️ **TLS 指纹风险 / 本机不可验（留真机）**：CF WAF 校验 TLS 指纹，rustls ClientHello ≠ okhttp
/// → 真实注册可能 1020/403（`runtime/http.rs` WarpHttp impl 文档已登记）。且真实注册有副作用（在 CF 建真
/// 设备）。本批只用 mock 验编排；真实 WARP 可用性未验。失败形态是明确的 `WARP_REGISTER_FAILED` error code
/// （body 携 CF `1020` 时不伪装成功），前端据此提示，而非静默降级。
#[tauri::command]
pub async fn warp_register(
    state: State<'_, AppRuntime>,
    license_key: Option<String>,
) -> Result<ApiResponse<Value>, ()> {
    let seed = match crate::runtime::mesh::generate_warp_seed() {
        Ok(s) => s,
        Err(e) => return Ok(ApiResponse::err_with_code(e, "WARP_RNG_UNAVAILABLE")),
    };
    let svc = crate::runtime::mesh::warp_service(state.http().clone(), seed);
    match svc.register(RegisterOptions { license_key }).await {
        Ok(draft) => match serde_json::to_value(&draft) {
            Ok(v) => Ok(ApiResponse::ok(v)),
            Err(e) => Ok(ApiResponse::err_with_code(
                format!("WARP 草稿序列化失败: {e}"),
                "WARP_DRAFT_SERIALIZE",
            )),
        },
        Err(e) => Ok(ApiResponse::err_with_code(e, "WARP_REGISTER_FAILED")),
    }
}

/// 上游 `WARP_APPLY_LICENSE`：对已注册 WARP 节点原地应用 WARP+ license（升级免重建）。
///
/// token/deviceId 服务端按 `server_id` 从 `wireguardSettings.warpDevice` 取，不经前端回传。无凭据的旧节点
/// 返 `{ok:false, error:"no-credentials"}`（真实业务结果，非 stub 假成功——对齐 上游 handler）。
/// 网络/许可失败返 `{ok:false, error}`；成功返 `{ok:true, warpPlus}`。信封恒 success（业务态在 data.ok）。
#[tauri::command]
pub async fn warp_apply_license(
    state: State<'_, AppRuntime>,
    server_id: String,
    license: String,
) -> Result<ApiResponse<Value>, ()> {
    let cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };
    // 取该节点已注册的 warpDevice 凭据（deviceId+token）；转 owned 以免借用跨 await。
    let dev = cfg
        .get("servers")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|s| s.get("id").and_then(Value::as_str) == Some(server_id.as_str()))
        })
        .and_then(|s| s.get("wireguardSettings"))
        .and_then(|w| w.get("warpDevice"));
    let device_id = dev
        .and_then(|d| d.get("deviceId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let token = dev
        .and_then(|d| d.get("token"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if device_id.is_empty() || token.is_empty() {
        return Ok(ApiResponse::ok(
            json!({ "ok": false, "error": "no-credentials" }),
        ));
    }
    // applyLicense 路径不触碰 keypair（`WarpService::apply_license` 从不调 generate_keypair）→ 占位种子。
    let svc = crate::runtime::mesh::warp_service(state.http().clone(), [0u8; 32]);
    match svc.apply_license(&device_id, &token, &license).await {
        Ok(acct) => Ok(ApiResponse::ok(
            json!({ "ok": true, "warpPlus": acct.warp_plus }),
        )),
        Err(e) => Ok(ApiResponse::ok(json!({ "ok": false, "error": e }))),
    }
}

/// 上游 `TAILSCALE_LOGIN`：拉起瞬态登录核抓交互登录 URL（Phase 2）。
///
/// 语义契约：`started:true` 仅表示**已起核**，非「已登录」——登录 URL 经 `event:tailscaleAuthUrl` 异步到达
/// （前端弹窗监听），真正登录成功要用户在浏览器完成。双写守卫命中（该 endpoint 已在运行主核）→
/// `{started:false, reason:'inMainCore'}`（复用 `tailscale_endpoint_in_running_core`，避两核同写 state 冲突）。
/// 起核前失败（配置解析/resolve/端口或 secret 解析/写盘/`sing-box check`/spawn/STATUS 订阅）→ 结构化 error
/// （信封 success=false + code）。
///
/// **URL 与登录成功都取自瞬态核自己的管理 API STATUS 流**（`authURL` / `backendState=Running`），核 stdout
/// 只进日志、不再是判据来源；gRPC 腿建不起来即硬失败，不回退 stdout（取舍与理由见
/// `runtime::tailscale_login_core` 模块头）。
///
/// **诚实边界（真机门槛）**：真 spawn + 连 Tailscale 控制面 + 真登录 URL 一段**在本机无法验证**（本仓禁跑
/// 触碰宿主网络的测试；`sing-box check` 只验配置形状，验不了运行时是否真吐 URL）。可单测面（注册表生命周期 /
/// 去重 / 超时 / 取消 / reap / STATUS→URL relay / Running→收核）已用 mock spawner + mock STATUS 流覆盖于
/// `runtime::tailscale_login_core`；端到端登录待真机会话，验收清单见
/// `~/docs/polaris/design/polaris-tailscale-login-wiring.md`。
#[tauri::command]
pub async fn tailscale_login(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    server: Value,
) -> Result<ApiResponse<Value>, ()> {
    let server_cfg: ServerConfig = match serde_json::from_value(server) {
        Ok(s) => s,
        Err(e) => {
            return Ok(ApiResponse::err_with_code(
                format!("Tailscale 登录：节点配置解析失败: {e}"),
                "TAILSCALE_LOGIN_BAD_SERVER",
            ))
        }
    };
    // 双写守卫入参：运行主核状态 + 运行配置（endpoint 已在运行主核则不再起瞬态核）。
    // `clash_api_port` 一并带上：瞬态核自己的管理 API 端口要避开主核那个（同一份运行快照，
    // 语义正好 = 此刻真的被占着的端口；核没跑时它是 0，`PortExclusions` 自会滤掉）。
    let status = state.proxy().status();
    let is_running = status.running;
    let running_cfg: Option<UserConfig> = state
        .proxy()
        .current_config_snapshot()
        .and_then(|v| serde_json::from_value(v).ok());
    match state
        .mesh()
        .start_tailscale_login(
            app,
            &server_cfg,
            is_running,
            running_cfg.as_ref(),
            status.clash_api_port,
        )
        .await
    {
        StartLoginOutcome::Started => Ok(ApiResponse::ok(json!({ "started": true }))),
        StartLoginOutcome::InMainCore => Ok(ApiResponse::ok(
            json!({ "started": false, "reason": "inMainCore" }),
        )),
        StartLoginOutcome::Failed(e) => Ok(ApiResponse::err_with_code(e, "TAILSCALE_LOGIN_FAILED")),
    }
}

/// 上游 `TAILSCALE_LOGIN_CANCEL`：取消某节点在飞的瞬态登录核（kill + 注销）。
/// 幂等：取消一个不存在的登录不算错（对齐 Polaris handler 的静默 ok）。
#[tauri::command]
pub fn tailscale_login_cancel(state: State<'_, AppRuntime>, server_id: String) -> ApiResponse<()> {
    let _ = state.mesh().cancel_tailscale_login(&server_id);
    ok_void()
}

/// 上游 `TAILSCALE_LOGOUT`：退出登录（清 state 目录；保留节点配置/authKey）。
///
/// `runningNeedsRestart`：该 TS endpoint 是否仍在**当前运行主核**内。1.14 always-emit 下「节点在运行配置里
/// = 已在主核」——登出只清了盘上 state 目录，运行中的主核仍持该 endpoint 的旧登录态，须重启核才真正生效
/// → 前端据此提示重启。判定复用 mesh crate `tailscale_endpoint_in_running_core`（与 login 双写守卫同真值源）。
#[tauri::command]
pub fn tailscale_logout(state: State<'_, AppRuntime>, server_id: String) -> ApiResponse<Value> {
    let _ = state.mesh().tailscale_logout(&server_id);
    let is_running = state.proxy().status().running;
    let running_cfg = state.proxy().current_config_snapshot();
    let needs_restart = logout_needs_restart(&server_id, is_running, running_cfg.as_ref());
    ApiResponse::ok(json!({ "runningNeedsRestart": needs_restart }))
}

/// `tailscale_logout.runningNeedsRestart` 装配判定：运行配置快照（JSON）反序列化后交 mesh crate
/// `tailscale_endpoint_in_running_core`。核未跑 / 快照缺失 / 结构不符 → false（保守：判不准不误报重启）。
fn logout_needs_restart(server_id: &str, is_running: bool, running_cfg: Option<&Value>) -> bool {
    if !is_running {
        return false;
    }
    let cfg: Option<UserConfig> = running_cfg.and_then(|v| serde_json::from_value(v.clone()).ok());
    polaris_mesh::tailscale_endpoint_in_running_core(server_id, is_running, cfg.as_ref())
}

/// 上游 `TAILSCALE_STATE_EXISTS`：批量查 TS 节点 state 目录存在性。
#[tauri::command]
pub fn tailscale_state_exists(
    state: State<'_, AppRuntime>,
    server_ids: Vec<String>,
) -> ApiResponse<Value> {
    let map = state.mesh().tailscale_state_exists(&server_ids);
    ApiResponse::ok(serde_json::to_value(map).unwrap_or_default())
}

/// 上游 `TAILSCALE_GET_STATUS`：拉各 TS 节点状态末帧（sing-box 管理 API STATUS 流缓存）。
///
/// **A3 已接线**：读 `MeshRuntime` 的 STATUS 末帧缓存（由 `proxy.rs::spawn_tailscale_status_relay` 在核就绪后
/// 订阅 `SubscribeTailscaleStatus` 流逐帧更新），`connected` = 主核是否在运行（= 状态流是否 live）。
/// - 核在跑且已收帧 → `{connected:true, statuses:[各在册 TS 节点末帧]}`（幽灵端点已在解码层过滤）。
/// - 核在跑但未收首帧 → `{connected:true, statuses:[]}`（流 live、尚无数据，诚实）。
/// - 核未跑 / 已停（缓存已清）→ `{connected:false, statuses:[]}`（renderer 据 connected=false 灰显动态位）。
///
/// `connected` 取 `proxy().status().running`（live 真值），缓存取 `mesh()`——二者在此合成（缓存本身不含 running）。
#[tauri::command]
pub fn tailscale_get_status(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    let running = state.proxy().status().running;
    let snapshot = state.mesh().tailscale_status_snapshot(running);
    ApiResponse::ok(
        serde_json::to_value(&snapshot)
            .unwrap_or_else(|_| json!({ "connected": false, "statuses": [] })),
    )
}

/// 删除节点副作用：tailscale state 清理 + WARP 远端注销入队（Polaris runServerRemovalSideEffects）。
fn run_server_removal_side_effects(state: &AppRuntime, removed: &Value) {
    // Tailscale 节点 → 清其 state 目录（best-effort）。
    let protocol = removed
        .get("protocol")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if protocol == "tailscale" {
        if let Some(id) = removed.get("id").and_then(Value::as_str) {
            let _ = state.mesh().tailscale_logout(id);
        }
    }
    // WARP 节点带自删凭据 → 入待注销队列（drain 由 `MeshRuntime::spawn_warp_drain` 启动期 + 定时消费）。
    // 入队/护栏/分类判定全在 mesh crate 纯逻辑；此处只把凭据递进 runtime 落盘。
    let warp_device = removed
        .get("wireguardSettings")
        .and_then(|w| w.get("warpDevice"));
    if let (Some(token), Some(device_id)) = (
        warp_device.and_then(|d| d.get("token").and_then(Value::as_str)),
        warp_device.and_then(|d| d.get("deviceId").and_then(Value::as_str)),
    ) {
        state.mesh().enqueue_warp_deregister(device_id, token);
    }
}

/// ISO8601 当前时间（上游 `new Date().toISOString()`）。
fn current_iso() -> String {
    // 复用 stats-engine 的 created_at_to_rfc3339（无 chrono/time 依赖的 civil 算法）；旧实现把整个 epoch 秒
    // 塞进秒字段产出非法 ISO（前端 Invalid Date），改为真 epoch-millis→RFC3339。时钟异常 → 空串，不 panic。
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .and_then(polaris_stats_engine::created_at_to_rfc3339)
        .unwrap_or_default()
}

/// UUID v4 生成（无 uuid crate 依赖时用伪随机占位——满足 id 唯一性语义）。
fn new_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pol-{nanos:032x}")
}

#[cfg(test)]
mod server_add_tests {
    use super::*;
    use crate::runtime::config::ConfigManager;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-server-add-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed_switch_nodes(mgr: &ConfigManager) {
        let mut cfg = mgr.load_full().unwrap();
        cfg["servers"] = json!([
            {"id":"n-a","name":"A","protocol":"trojan","address":"1.1.1.1","port":443,"password":"pw"},
            {"id":"n-b","name":"B","protocol":"trojan","address":"2.2.2.2","port":443,"password":"pw"},
            {"id":"n-c","name":"C","protocol":"trojan","address":"3.3.3.3","port":443,"password":"pw"}
        ]);
        cfg["selectedServerId"] = json!("n-a");
        mgr.save_full(&cfg).unwrap();
    }

    #[test]
    fn server_switch_core_updates_selection_and_mru_in_one_write() {
        let dir = temp_dir("switch-core");
        let mgr = ConfigManager::new(dir.clone());
        seed_switch_nodes(&mgr);

        let (cfg, changed) = server_switch_core(&mgr, "n-b").expect("切换应成功");
        assert!(changed);
        assert_eq!(cfg["selectedServerId"], json!("n-b"));
        assert_eq!(cfg["recentServerIds"], json!(["n-b"]));
        assert_eq!(mgr.load_full().unwrap()["selectedServerId"], json!("n-b"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 调用点门：ConfigManager::update 已有强制交错的并发行为测；这里补生产入口接线，避免
    /// `server_switch_core` 被改回 load_full → save_full 分离三步后那条底层测试仍然假绿。
    #[test]
    fn server_switch_core_uses_atomic_config_update() {
        let src = include_str!("server.rs");
        let start = src.find("fn server_switch_core(").expect("核心锚点");
        let end = src[start..]
            .find("/// 上游 `SERVER_SWITCH`")
            .map(|n| start + n)
            .expect("命令锚点");
        let body = &src[start..end];
        assert!(body.contains(".update(|cfg|"), "切节点必须走原子 update");
        assert!(!body.contains("load_full()"), "不得退回分离读改写");
        assert!(!body.contains("save_full("), "不得退回分离读改写");
    }

    /// A7 · 删节点后选中出口的旧→新转换 → `selected_exit_changed` 判失效（server_delete / batch 共用腿的牙）。
    /// 打断 `apply_selection_fallback`（不改选中）→「删选中 → 变」断言转红；打断 `selected_exit_changed` → 转红。
    /// cfg 传**删后 servers**（viable 校验看删后集）。此腿走哨兵/兜底，不产生 →null（→null 由订阅腿覆盖）。
    #[test]
    fn delete_selection_transition_signals_exit_change() {
        // 删当前选中 + 有存活兜底 → 选中回落兜底（!= 旧 id）→ 出口变。
        let mut cfg = json!({ "selectedServerId": "a", "servers": [{ "id": "b" }] });
        let old = cfg["selectedServerId"].as_str().map(str::to_string);
        apply_selection_fallback(&mut cfg, true, Some("b"));
        let new = cfg.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(new, Some("b"), "存活兜底 → 采用");
        assert!(
            selected_exit_changed(old.as_deref(), new),
            "删选中 → 出口变（失效）"
        );

        // 删当前选中 + 无存活兜底（删光）→ 回落直连哨兵（!= 旧 id）→ 仍出口变。
        let mut cfg2 = json!({ "selectedServerId": "a", "servers": [] });
        let old2 = cfg2["selectedServerId"].as_str().map(str::to_string);
        apply_selection_fallback(&mut cfg2, true, None);
        let new2 = cfg2.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(new2, Some(DIRECT_SERVER_ID), "无兜底 → 直连哨兵");
        assert!(
            selected_exit_changed(old2.as_deref(), new2),
            "删光选中 → 出口变（失效）"
        );

        // 删非选中节点 → 选中不动 → 出口不变（守卫白刷）。
        let mut cfg3 = json!({ "selectedServerId": "a", "servers": [{ "id": "a" }] });
        let old3 = cfg3["selectedServerId"].as_str().map(str::to_string);
        apply_selection_fallback(&mut cfg3, false, Some("a"));
        let new3 = cfg3.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(new3, Some("a"), "删非选中 → 选中不动");
        assert!(
            !selected_exit_changed(old3.as_deref(), new3),
            "删非选中 → 出口不变（不失效）"
        );
    }

    #[test]
    fn ensure_server_id_mints_when_missing_or_empty_keeps_existing() {
        let minted = ensure_server_id(json!({ "name": "n", "protocol": "vless" }));
        assert!(
            minted["id"].as_str().is_some_and(|s| !s.is_empty()),
            "缺 id → mint"
        );
        let empty = ensure_server_id(json!({ "id": "", "name": "n" }));
        assert!(
            empty["id"].as_str().is_some_and(|s| !s.is_empty()),
            "空 id → mint"
        );
        let kept = ensure_server_id(json!({ "id": "keep-me", "name": "n" }));
        assert_eq!(kept["id"], json!("keep-me"), "已有 id → 保留");
    }

    /// 真实 ConfigStore 端到端：无 id 的手动节点经 server_add_core → 落盘带新 id 且**存活**（有 id 才不被
    /// sanitize 丢弃）。回归到「直接 push 不补 id」→ sanitize 丢弃 → servers.len()==0，此测转红。
    #[test]
    fn server_add_core_persists_node_with_minted_id() {
        let dir = temp_dir("persist");
        // 完整可过 sanitize/validate 的 trojan 节点，但无 id。
        let node = json!({
            "name": "手动节点",
            "protocol": "trojan",
            "address": "1.2.3.4",
            "port": 443,
            "password": "pw",
        });
        {
            let mgr = ConfigManager::new(dir.clone());
            server_add_core(&mgr, node).expect("server_add_core 应成功");
        }
        // 重 load 自磁盘。
        let mgr2 = ConfigManager::new(dir.clone());
        let cfg = mgr2.load_full().unwrap();
        let servers = cfg["servers"].as_array().unwrap();
        assert_eq!(
            servers.len(),
            1,
            "落盘节点存活（有 id 才不被 sanitize 丢弃）"
        );
        assert!(
            servers[0]["id"].as_str().is_some_and(|s| !s.is_empty()),
            "落盘节点带非空 id"
        );
        assert_eq!(servers[0]["name"], json!("手动节点"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D4 兜底出口的 viable 校验（单删/批删共用）：传入的兜底 id 必须**存活于删除后的 servers**。
    /// 回归到「前端传被删节点自身 id」→ 该 id 已不在 servers → 必须落直连哨兵而非把死 id 写回选中。
    /// A5：logout runningNeedsRestart 装配判定。核未跑 / 快照缺失 → false；运行中且该 TS 节点在运行配置里 → true。
    /// 回归到硬编码 false（旧态）或打断 crate 调用 → 「true」用例转红。
    #[test]
    fn logout_needs_restart_true_only_when_ts_in_running_core() {
        use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};

        // 运行配置含 id=ts1 的 tailscale 节点（round-trip 保证可反序列化）。
        let mut cfg = UserConfig::default();
        cfg.servers.push(ServerConfig {
            id: "ts1".into(),
            protocol: Protocol::Tailscale,
            ..Default::default()
        });
        let running = serde_json::to_value(&cfg).unwrap();

        assert!(
            logout_needs_restart("ts1", true, Some(&running)),
            "运行中 + TS 节点在运行核 → 需重启"
        );
        assert!(
            !logout_needs_restart("other", true, Some(&running)),
            "节点不在运行核 → 无需重启"
        );
        assert!(
            !logout_needs_restart("ts1", false, Some(&running)),
            "核未跑 → 无需重启"
        );
        assert!(
            !logout_needs_restart("ts1", true, None),
            "运行配置快照缺失 → 保守 false"
        );
    }

    /// 项2（server:addBulk 返回实际新增数）：核心须返回 `added == 入参 len`（对齐 上游 `added=list.length`），
    /// 且节点真落盘。变异牙：回归到硬编码 0（`Ok((cfg, 0))`）→「added==3」断言转红；打断 push/init 落盘 →
    /// 重 load 的 servers.len() 断言转红。
    #[test]
    fn server_add_bulk_core_returns_actual_added_count() {
        let dir = temp_dir("bulk");
        // 3 条可过 sanitize/validate 的 trojan 节点（无 id，addBulk 强制 mint）。
        let nodes = vec![
            json!({"name":"A","protocol":"trojan","address":"1.1.1.1","port":443,"password":"pw"}),
            json!({"name":"B","protocol":"trojan","address":"2.2.2.2","port":443,"password":"pw"}),
            json!({"name":"C","protocol":"trojan","address":"3.3.3.3","port":443,"password":"pw"}),
        ];
        let expected = nodes.len();
        {
            let mgr = ConfigManager::new(dir.clone());
            let (_cfg, added) =
                server_add_bulk_core(&mgr, nodes).expect("server_add_bulk_core 应成功");
            assert_eq!(
                added, expected,
                "返回实际新增数（= 入参 len），此前硬编码 0"
            );
        }
        // 重 load 自磁盘核实：3 条自建节点存活。
        let cfg = ConfigManager::new(dir.clone()).load_full().unwrap();
        let servers = cfg["servers"].as_array().unwrap();
        assert_eq!(servers.len(), expected, "批量节点全部落盘存活");
        assert!(
            servers
                .iter()
                .all(|s| s["id"].as_str().is_some_and(|i| !i.is_empty())),
            "每个落盘节点带非空 mint id"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 托盘 MRU 入队（`push_recent_server_id`）：去重插队首 + 上限 3。牙：打断 `retain` 去重 → 重选同一
    /// 节点会在列表中重复出现，断言转红；打断 `truncate(3)` → 第 4 项断言转红；打断 `insert(0,..)`
    /// （改用 push 到队尾）→ 「最近一次在最前」断言转红。
    #[test]
    fn push_recent_server_id_dedupes_and_caps_at_three() {
        let mut obj = Map::new();
        push_recent_server_id(&mut obj, "a");
        push_recent_server_id(&mut obj, "b");
        push_recent_server_id(&mut obj, "c");
        assert_eq!(
            obj["recentServerIds"],
            json!(["c", "b", "a"]),
            "最近一次切换的节点在队首"
        );

        // 重选已在历史里的节点（"a"）→ 去重后提到队首，而非重复出现。
        push_recent_server_id(&mut obj, "a");
        assert_eq!(
            obj["recentServerIds"],
            json!(["a", "c", "b"]),
            "重选历史节点 → 去重后提到队首"
        );

        // 第 4 个不同节点 → 最老的一条（"b"）被挤出，上限恒为 3。
        push_recent_server_id(&mut obj, "d");
        assert_eq!(
            obj["recentServerIds"],
            json!(["d", "a", "c"]),
            "上限 3：最老条目被挤出"
        );
    }

    /// 删节点须把死 id 从 MRU 剔除 —— 否则它永久占住 `truncate(3)` 的一个槽位，
    /// 而 `TrayMenu` 反查不到节点即跳过且不回填 ⇒「节点·最近」恒少显示一条。
    ///
    /// 牙：把 `arr.retain(...)` 整条删掉（或把谓词取反）→ 前两个断言转红。
    #[test]
    fn prune_recent_server_ids_drops_deleted_and_keeps_survivors() {
        // 单删形态：只剔被删的那个，其余保序。
        let mut cfg = json!({ "recentServerIds": ["a", "b", "c"] });
        prune_recent_server_ids(&mut cfg, |id| id == "b");
        assert_eq!(
            cfg["recentServerIds"],
            json!(["a", "c"]),
            "只剔被删 id，其余保序"
        );

        // 批删形态：一次剔多个。
        let mut cfg = json!({ "recentServerIds": ["a", "b", "c"] });
        let removed: std::collections::HashSet<&str> = ["a", "c"].into_iter().collect();
        prune_recent_server_ids(&mut cfg, |id| removed.contains(id));
        assert_eq!(cfg["recentServerIds"], json!(["b"]), "批删一次剔多个");

        // 全删 → 空数组（**不删键**，形状稳定；空数组与缺键对读侧 `?? []` 等价）。
        let mut cfg = json!({ "recentServerIds": ["a"] });
        prune_recent_server_ids(&mut cfg, |_| true);
        assert_eq!(cfg["recentServerIds"], json!([]), "全删 → 空数组而非删键");

        // 存量配置无该键 → 空操作，**不得凭空建键**（否则每次删节点都往 config 里塞一个空数组）。
        let mut cfg = json!({ "servers": [] });
        prune_recent_server_ids(&mut cfg, |_| true);
        assert!(cfg.get("recentServerIds").is_none(), "无该键 → 不凭空建键");
    }

    /// **调用点守卫**（射程补齐）：上面那条只测纯函数，删掉命令里的**调用**它照样绿 = 门没盖住生产路径。
    /// 两个删除命令都持 `State<'_, AppRuntime>`，单测构造不出 Tauri 运行时 ⇒ 改用源码扫描锁调用点，
    /// 与 `main.rs` 既有的 Rust 侧源码扫描守卫同法。
    ///
    /// 牙：删掉 `server_delete` 或 `server_delete_batch` 里任一处 `prune_recent_server_ids(...)` → 转红。
    #[test]
    fn both_delete_commands_prune_recent_ids() {
        let src = include_str!("server.rs");
        // 扫描面自检：锚点必须都在，否则函数改名 / 文件漂走会让下面的切片恒空、守卫恒绿。
        for anchor in [
            "pub fn server_delete(",
            "pub fn server_delete_batch(",
            "pub fn server_get_all(",
        ] {
            assert!(src.contains(anchor), "锚点消失，守卫已失去判据: {anchor}");
        }

        // 取每个命令的函数体（从其签名到**下一个**命令签名之间）。
        let body_of = |start: &str, end: &str| -> &str {
            let s = src.find(start).expect("起始锚点");
            let e = src[s..].find(end).expect("结束锚点") + s;
            &src[s..e]
        };
        let single = body_of("pub fn server_delete(", "pub fn server_delete_batch(");
        let batch = body_of("pub fn server_delete_batch(", "pub fn server_get_all(");

        assert!(
            single.contains("prune_recent_server_ids("),
            "server_delete 必须剔除被删节点的 MRU 残留"
        );
        assert!(
            batch.contains("prune_recent_server_ids("),
            "server_delete_batch 必须剔除被删节点的 MRU 残留"
        );
        // 反向自检：切片确实各自独立（否则「两段都命中」可能只是因为切成了同一段整文件）。
        assert!(!single.contains("pub fn server_delete_batch("));
        assert!(!batch.contains("pub fn server_get_all("));
    }

    #[test]
    fn resolve_fallback_selected_requires_surviving_candidate() {
        // 删除已发生：servers 里只剩 keep-1 / keep-2。
        let cfg = json!({ "servers": [{ "id": "keep-1" }, { "id": "keep-2" }] });
        assert_eq!(
            resolve_fallback_selected(&cfg, Some("keep-2")),
            "keep-2",
            "兜底节点存活 → 采用"
        );
        assert_eq!(
            resolve_fallback_selected(&cfg, Some("deleted-1")),
            DIRECT_SERVER_ID,
            "兜底节点已被删（含「传被删节点自身」的旧 bug 形态）→ 落直连哨兵"
        );
        assert_eq!(
            resolve_fallback_selected(&cfg, None),
            DIRECT_SERVER_ID,
            "无兜底候选（删光了）→ 落直连哨兵"
        );
        assert_eq!(
            resolve_fallback_selected(&json!({}), Some("keep-1")),
            DIRECT_SERVER_ID,
            "servers 字段缺失 → 无可校验候选 → 落直连哨兵（不写回未经校验的 id）"
        );
    }
}
