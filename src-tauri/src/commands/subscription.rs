//! 订阅类 command（上游 `subscription-handlers.ts`）。
//!
//! 映射 channel：
//! - `subscription:add` → [`subscription_add`]
//! - `subscription:update` → [`subscription_update`]
//! - `subscription:delete` → [`subscription_delete`]
//! - `subscription:updateServers` → [`subscription_update_servers`]（net-stack 拉取 + 对账 + force-restart）
//! - `subscription:preview` → [`subscription_preview`]（net-stack 预检，不写 config）
//! - `localImport:parse` → [`local_import_parse`]（net-stack 离线解析）
//! - `localImport:pickFile` → [`local_import_pick_file`]（Tauri dialog 插件）

#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use polaris_config_engine::builder::endpoint_routes::{
    is_mesh_node_unroutable, mesh_node_carries_full_tunnel,
};
use polaris_config_engine::user_config::dns_constants::{is_direct_selection, DIRECT_SERVER_ID};
use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};
use polaris_net_stack::clash_parser::default_max_providers;
use polaris_net_stack::singbox_import::ImportOrigin;
use polaris_net_stack::ssrf::DnsLookup;
use polaris_net_stack::subscription::{
    dedupe_by_fingerprint, default_subscription_user_agent, extract_proxy_providers,
    fetch_subscription_full, fetch_subscription_with_meta, parse_subscription,
    resolve_proxy_providers, Conditional, ProviderFetchError, MAIN_FETCH_TIMEOUT_MS,
    PROVIDER_FETCH_TIMEOUT_MS,
};
use polaris_net_stack::subscription_error::SubscriptionErrorKind;

use crate::commands::config::broadcast_config_changed;
use crate::events::{broadcast, channel::EVENT_SUBSCRIPTION_UPDATE_PROGRESS};
use crate::i18n::{key, t};
use crate::response::{ok_void, ApiResponse};
use crate::runtime::http::{HttpRuntime, SystemDnsLookup};
use crate::runtime::unlock::{selected_exit_changed, BroadcastSink};
use crate::runtime::AppRuntime;

/// 上游 `SUBSCRIPTION_ADD`：新增订阅（生成 id + createdAt，写 config）。
#[tauri::command]
pub fn subscription_add(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    subscription: Value,
) -> ApiResponse<Value> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let id = new_uuid();
    let mut sub = subscription;
    if let Some(obj) = sub.as_object_mut() {
        obj.insert("id".to_string(), json!(id));
        obj.insert("createdAt".to_string(), json!(current_iso()));
    }
    let new_sub = sub.clone();
    if let Some(arr) = cfg.get_mut("subscriptions").and_then(Value::as_array_mut) {
        arr.push(sub);
    } else if let Some(obj) = cfg.as_object_mut() {
        obj.insert("subscriptions".to_string(), Value::Array(vec![sub]));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ApiResponse::ok(new_sub)
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 条件 GET 验证器是否随本次编辑作废：**per-sub UA 变了就作废**。
///
/// # 为什么（`DESIGN-REVIEW(ua-invalidates-validators)` 的落地）
///
/// `etag`/`lastModified` 是**上一次响应**的验证器，而机场普遍**按 UA 下发不同变体**
/// （clash / sing-box / base64 三套正文，同一个 URL）。若 ETag 是按「订阅版本」而非「响应变体」
/// 生成的（相当常见），那么换了 UA 之后带旧验证器再请求，服务端照样回 **304** ——
/// 我们短路 parse/reconcile，**新格式永远拿不到**，用户只看到「无变化」且无从排查。
///
/// 判据只看 UA 是否**真的变了**（`None`/`""`/纯空白按同一口径归一，与
/// [`resolve_subscription_ua`] 的 falsy 语义一致）：URL/名字等其它字段的编辑不该白扔验证器。
/// URL 变了不需要在此处理 —— 那是另一个资源，服务端本来就不会拿旧 ETag 判 304。
///
/// **本函数只管 per-sub 那一级**。全局 `config.subscriptionUserAgent` 改动走 config 写命令、
/// 不经本函数 → 那条腿由 [`invalidate_validators_on_global_ua_change`] 在 config 写入侧收口
/// （`commands/config.rs` 的两个写命令各挂一处）。二者判据同源（都走 [`pick_ua`] 归一 +
/// [`resolve_subscription_ua`] 的优先级），合起来覆盖两级 UA 的全部变更面。
fn ua_changed(old_sub: &Value, new_sub: &Value) -> bool {
    pick_ua(old_sub.get("userAgent")) != pick_ua(new_sub.get("userAgent"))
}

/// 全局订阅 UA 的 config 键名（**单一真值**：解析、变更判定、config 写侧路由三处共用一个字面量）。
pub(crate) const SUBSCRIPTION_USER_AGENT_KEY: &str = "subscriptionUserAgent";

/// UA 取值归一（**falsy** 语义）：缺省 / 非字符串 / `""` / 纯空白 一律折成 `None`。
///
/// 全仓凡是「这个 UA 算不算设了」的判断都必须经本函数 —— 三处（解析优先级、per-sub 变更判定、
/// 全局变更判定）各写一遍 `trim().is_empty()` 迟早漂移，而漂移的表现是**恒 304**这种无声故障。
fn pick_ua(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 全局 `subscriptionUserAgent` 变更 → 作废**受影响订阅**的条件 GET 验证器。就地改 `new_cfg`，
/// 返回真被清掉验证器的订阅条数（供调用方记日志 / 测试断言）。
///
/// # 为什么必须有（与 [`ua_changed`] 是同一个洞的另一半）
///
/// `etag`/`lastModified` 是**上一次响应**的验证器，而机场普遍按 UA 下发不同正文变体
/// （clash / sing-box / base64 三套，同一个 URL）。ETag 若按「订阅版本」而非「响应变体」生成
/// （相当常见），换 UA 后带旧验证器再请求照样回 **304** ⇒ 我们短路 parse/reconcile，
/// **新格式永远拿不到**，用户只看到「无变化」且无从排查。per-sub UA 那一级已由 [`ua_changed`]
/// 在 `subscription_update` 里收口；全局这一级此前**无任何消费/清理点**（全仓实证），
/// 于是「改了全局 UA 仍恒 304」是一条完全没人管的腿。
///
/// # 射程为什么不是「全部订阅一把清」
///
/// 判据是**生效 UA 是否变**，不是「全局键是否变」：带 per-sub `userAgent` 覆盖的订阅，其生效 UA
/// 由第一级决定（见 [`resolve_subscription_ua`]），全局怎么改都不影响它 ⇒ 它的验证器仍然有效，
/// 清掉只会白扔一次条件 GET、把下次更新变成全量下载。所以逐订阅按 `per-sub ?? 全局` 折算前后两值再比。
///
/// 归一口径与 [`pick_ua`] 一致：`None` / `""` / 纯空白三者互相之间**不算变更**（用户把设置框清空
/// 再存回，不该把全部订阅的验证器扔掉）。
pub(crate) fn invalidate_validators_on_global_ua_change(
    old_cfg: &Value,
    new_cfg: &mut Value,
) -> usize {
    let old_global = pick_ua(old_cfg.get(SUBSCRIPTION_USER_AGENT_KEY));
    let new_global = pick_ua(new_cfg.get(SUBSCRIPTION_USER_AGENT_KEY));
    // 全局键没动 → 任何订阅的生效 UA 都不可能因此变。早退，零遍历、零改动。
    if old_global == new_global {
        return 0;
    }
    let Some(arr) = new_cfg
        .get_mut("subscriptions")
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    let mut cleared = 0usize;
    for sub in arr.iter_mut() {
        // 生效 UA 折算：per-sub 优先（`resolve_subscription_ua` 第一级），缺省才回落全局。
        let per_sub = pick_ua(sub.get("userAgent"));
        if per_sub.is_some() {
            continue; // per-sub 覆盖 → 生效 UA 与全局无关，验证器仍有效
        }
        // 计数只算「真有验证器被扔掉」的那些，日志才不会谎报条数。
        let had = sub.get("etag").is_some() || sub.get("lastModified").is_some();
        set_or_remove(sub, "etag", None);
        set_or_remove(sub, "lastModified", None);
        if had {
            cleared += 1;
        }
    }
    cleared
}

/// 上游 `SUBSCRIPTION_UPDATE`：更新订阅元数据（按 id）。
///
/// **前端整体替换该记录**（`SubDialog` 用 `{...base, …}` 回传），故 etag/lastModified 等后端字段
/// 靠 spread 幸存 —— 唯独 UA 变更必须主动作废验证器，见 [`ua_changed`]。
#[tauri::command]
pub fn subscription_update(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    subscription: Value,
) -> ApiResponse<()> {
    let id = subscription
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let id = match id {
        Some(i) => i,
        None => return ApiResponse::err("subscription.id required"),
    };
    let mut subscription = subscription;
    let found = cfg
        .get_mut("subscriptions")
        .and_then(Value::as_array_mut)
        .map(|arr| {
            if let Some(idx) = arr
                .iter()
                .position(|s| s.get("id").and_then(Value::as_str) == Some(&id))
            {
                if ua_changed(&arr[idx], &subscription) {
                    // UA 换了 = 响应变体可能整个换掉 → 旧验证器作废，下次走全量 GET。
                    set_or_remove(&mut subscription, "etag", None);
                    set_or_remove(&mut subscription, "lastModified", None);
                }
                arr[idx] = subscription;
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if !found {
        return ApiResponse::err(format!("订阅不存在: {id}"));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 删订阅的纯 `Value` 变换（订阅本身 + 其下全部节点 + 悬挂选中置 null）。
///
/// 返回 `Err(())` = 订阅不存在（`subscriptions` 数组在但无此 id；命令层报 err）。`subscriptions` 字段整体缺失
/// → 视为无副作用的 `Ok`（对齐原命令：`if let Some(arr)` 直接跳过、不报错）。
///
/// A7：选中节点落在被删订阅下 → 该 id 从 servers 消失 → `selectedServerId` 置 `DIRECT_SERVER_ID` 哨兵
/// （**绝不裸 null**）。上游 删订阅置 null 后由 generate 视 null=direct；上游 `generate.rs:219` 对
/// **非哨兵 null** 报 `Selected server not found`（→ 删订阅后下次热切换必失败），故置显式 direct 哨兵达成
/// 等价的「直连」终态且不触发该回归。命令层落盘后用 `selected_exit_changed(old, new)` 判失效。
fn apply_subscription_delete(cfg: &mut Value, subscription_id: &str) -> Result<(), ()> {
    // 删订阅本身。
    if let Some(arr) = cfg.get_mut("subscriptions").and_then(Value::as_array_mut) {
        if let Some(idx) = arr
            .iter()
            .position(|s| s.get("id").and_then(Value::as_str) == Some(subscription_id))
        {
            arr.remove(idx);
        } else {
            return Err(());
        }
    }
    // 删该订阅下全部节点。
    if let Some(arr) = cfg.get_mut("servers").and_then(Value::as_array_mut) {
        arr.retain(|s| s.get("subscriptionId").and_then(Value::as_str) != Some(subscription_id));
    }
    // 选中被删 → 置 direct 哨兵（订阅删除路径：直连终态，对齐 上游 删订阅 null→direct 语义，
    // 但用哨兵避开 generate 对裸 null 的 `Selected server not found` 回归）。
    let selected_gone = cfg
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(|sid| {
            cfg.get("servers")
                .and_then(Value::as_array)
                .map(|arr| {
                    !arr.iter()
                        .any(|s| s.get("id").and_then(Value::as_str) == Some(sid))
                })
                .unwrap_or(true)
        })
        .unwrap_or(false);
    if selected_gone {
        if let Some(obj) = cfg.as_object_mut() {
            // 绝不裸 null（避免 generate.rs `Selected server not found` 回归）：置 direct 哨兵 = 显式直连。
            obj.insert("selectedServerId".to_string(), json!(DIRECT_SERVER_ID));
        }
    }
    Ok(())
}

/// 上游 `SUBSCRIPTION_DELETE`：删除订阅 + 其下全部节点 + 修选中（删选中 → null）。
#[tauri::command]
pub fn subscription_delete(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    subscription_id: String,
) -> ApiResponse<()> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    // A7：落盘前快照旧选中出口（选中落在被删订阅下 → 置 null = 出口变，作废旧出口解锁缓存）。
    let old_selected = cfg
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(str::to_string);
    if apply_subscription_delete(&mut cfg, &subscription_id).is_err() {
        return ApiResponse::err(format!("订阅不存在: {subscription_id}"));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            // A7：删订阅令选中节点从列表消失 → selectedServerId 变 null = 出口变 → 作废旧出口解锁探测缓存
            // （否则解锁角标最长陈旧 30min）。选中不属该订阅（仍存活）→ 出口不动、不失效。
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

/// 订阅更新成功结果（前端 `updateServers` 契约）。`unchanged`（304 / 内容等价）时 `true`；
/// `user_info`（本次流量/到期）有则透传供前端流量条即时刷新。
fn update_ok(
    added: usize,
    updated: usize,
    deleted: usize,
    unchanged: bool,
    user_info: Option<&Value>,
) -> Value {
    let mut v = json!({
        "success": true,
        "addedServers": added,
        "updatedServers": updated,
        "deletedServers": deleted,
        "unchanged": unchanged,
    });
    if let Some(ui) = user_info {
        v["userInfo"] = ui.clone();
    }
    v
}

/// 订阅更新的业务失败态（`success:false` + error，信封仍 ok —— 对齐旧 stub 与前端「读 data.success」）。
fn update_failure(error: impl Into<String>) -> Value {
    json!({
        "success": false,
        "addedServers": 0,
        "updatedServers": 0,
        "deletedServers": 0,
        "error": error.into(),
    })
}

/// 订阅「经代理更新」生效求值（上游 `resolveSubscriptionViaProxy`，`shared/subscription-proxy.ts`）。
///
/// 全局三态 `subscriptionProxyPolicy`（默认 `follow`）：
/// - `proxy`：所有订阅强制经代理，**忽略** per-sub；
/// - `direct`：所有订阅强制直连，**忽略** per-sub；
/// - `follow`（默认/未知值）：按 per-sub `updateViaProxy` 决定（默认 false=直连）。
pub(crate) fn resolve_subscription_via_proxy(
    policy: Option<&str>,
    sub_enabled: Option<bool>,
) -> bool {
    match policy {
        Some("proxy") => true,
        Some("direct") => false,
        _ => sub_enabled == Some(true), // follow（默认）
    }
}

/// 从整 config + 单订阅 Value 求值本次拉取是否经代理（C14 接线点）。
///
/// 读全局 `subscriptionProxyPolicy` 与该订阅 `updateViaProxy`，委托 [`resolve_subscription_via_proxy`]。
/// 独立成函数以便纯单测覆盖「配置键提取 + 三态决策」，无需真拉订阅（真经代理出口属真机门）。
fn want_proxy_for_sub(cfg: &Value, sub: &Value) -> bool {
    let policy = cfg.get("subscriptionProxyPolicy").and_then(Value::as_str);
    let sub_enabled = sub.get("updateViaProxy").and_then(Value::as_bool);
    resolve_subscription_via_proxy(policy, sub_enabled)
}

/// 全局策略是否**显式强制**经代理（`subscriptionProxyPolicy == "proxy"`）。
///
/// 与 [`want_proxy_for_sub`] 的区别是「意图强度」：`follow` 下的 `updateViaProxy=true` 是**偏好**
/// （上游 与本仓一致：端口不可用就退直连，自举友好）；`proxy` 是用户在设置页显式勾的
/// **全局强制**——它的语义是「订阅地址不许明文出网」。这两者不能共用一个静默回退。
fn proxy_policy_is_forced(cfg: &Value) -> bool {
    cfg.get("subscriptionProxyPolicy").and_then(Value::as_str) == Some("proxy")
}

/// 本次拉取的订阅 UA（三级优先级，**契约单一真值**）：
/// `subscription.userAgent` → 全局 `config.subscriptionUserAgent` → `None`（交
/// [`fetch_parse_resolve`] 落 `default_subscription_user_agent`）。
///
/// 契约声明见 `ui/src/contracts/types.ts`（per-sub 字段注释 + `subscriptionUserAgent` 字段）；
/// 上游 两条路径同式（`subscription-handlers.ts` 手动更新 + `SubscriptionScheduler` 自动更新，
/// 均为 `sub.userAgent ?? config.subscriptionUserAgent`）。
///
/// 此前后端**只读 per-sub**、全局键零消费 → 全局 UA 是死键：机场按 UA 下发不同格式（clash/sing-box/
/// base64），用户设了全局 UA 却不生效 → 拿到错格式或 0 节点。
///
/// # 与 TS `??` 的**已知差异**（登记，不是「对齐」）
///
/// 契约注释与 上游 写的都是 `sub.userAgent ?? config.subscriptionUserAgent`——**nullish** 合并：
/// 显式 `""` 是非 nullish 值，会**胜出**并把空 UA 发出去。本实现用的是 `trim().is_empty()` 过滤
/// = **falsy** 语义：空串/纯空白一律视同未设，继续向下一级回落。
///
/// 差异只在「per-sub 显式存了空串」这一格，且本实现的行为更可取：把 `User-Agent: ` 空值真发出去，
/// 机场侧多半按未知客户端处理 → 拿到错格式或 0 节点，而用户在对话框里清空输入框的意图显然是
/// 「不要 per-sub 覆盖」而非「发一个空 UA」。前端 `SubDialog` 也确实把空输入折成 `undefined`
/// （`ua.trim() || undefined`），故这一格在真实链路上几乎不可达 —— 但**导入的配置/手改的 json
/// 可以造出它**，所以行为差异必须写在这里，而不是自称「对齐 TS falsy 语义」（TS 那边是 `??`，
/// 不是 `||`，原注释名不副实）。差异已同步登记在 `ui/src/contracts/types.ts` 的 `userAgent` 字段注释。
fn resolve_subscription_ua(cfg: &Value, sub: &Value) -> Option<String> {
    pick_ua(sub.get("userAgent")).or_else(|| pick_ua(cfg.get(SUBSCRIPTION_USER_AGENT_KEY)))
}

/// 上游 `SUBSCRIPTION_UPDATE_SERVERS`：拉取订阅 + 对账节点集（net-stack）+ 持久化。
///
/// **已接线**（此前为「HTTP 栈依赖未拍板」占位；该结论已过时——`runtime/http.rs` 的 `HttpRuntime`
/// = reqwest+rustls，已实现 net-stack [`HttpClient`]，`subscription_preview` 两函数之外即在用）：
///
/// 1. 取该订阅的 `url`/`userAgent`，并按**全局三态策略** `subscriptionProxyPolicy` × per-sub
///    `updateViaProxy` 求值本次经代理与否（C14；[`want_proxy_for_sub`]）；
/// 2. 经 [`HttpRuntime`] + [`fetch_subscription_full`](polaris_net_stack::subscription::fetch_subscription_full)
///    （SSRF 逐跳 guard + 体积闸 + 超时 + 手动重定向，与 preview **同一** 拉取层）拉取；
/// 3. [`parse_subscription`](polaris_net_stack::subscription::parse_subscription) 解析（节点带本订阅 id）；
/// 4. **reconcile**（[`reconcile_subscription_servers`]）：五元组指纹（protocol/address/port/cred/network，
///    **剔除 name**，见 [`node_fingerprint`]）稳定对账——
///    命中保留原 id+createdAt（选中节点/用户身份不丢）、新增补入、消失删除；他订阅/自建节点不动；
/// 5. `save_full` 持久化 + 广播 `config:changed`（汇流点自动热切换判定）。
///
/// **空集/拉取失败走 merge-only**（不 permanent 删存量节点）：拉取半通或解析 0 节点时**不对账删除**，
/// 如实报业务失败（对齐旧占位注释的破坏性告警——差集不可信时绝不删节点）。
#[tauri::command]
pub async fn subscription_update_servers(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    subscription_id: String,
) -> Result<ApiResponse<Value>, ()> {
    // 命令与 scheduler 共用同一份拉取+对账+落盘核心（唯一生产路径，§K7.1）。
    Ok(ApiResponse::ok(
        perform_subscription_update(&app, state.inner(), &subscription_id).await,
    ))
}

/// 选择本次拉取的 HTTP client + 是否**实际**经代理。返回 `(client, via_effective)`。
///
/// `want_proxy` 且核在跑且 update-in socks 口有效 → 经 **update-in** 借道（隔离于主流量策略，对齐 上游
/// pin update-in socks 而非 mixed_port）；否则直连（不假装）。
///
/// **scheme 已对齐**（原 review-queue 条目 `sub-update-in-scheme`，已闭合）：改用
/// [`HttpRuntime::via_local_socks_proxy`]（`socks5://`），对齐 上游 `UpdateNetwork` 的
/// `proxyRules: socks5://127.0.0.1:<update-in>`。此前用的是 `via_local_proxy`（`http://`），而
/// `update-in` 是 sing-box `type:"socks"` 入站 → 明文 HTTP 打 socks 服务器首字节就对不上、必断连
/// → 本条链**恒失败**。`icon_cache` 同一口同一错，一并改。
///
/// **注意别改回共用一个构造器**：`speedtest` 的探测池 `probe-in-k` 是纯 `http` 入站，socks5 打不通；
/// 两个 scheme 各有其入站，见 [`HttpRuntime::via_local_socks_proxy`] 文档的入站对照表。
/// 协议握手由 `runtime::http` 的 `via_local_socks_proxy_really_speaks_socks5_to_a_socks_inbound`
/// 单测钉住；「真核 update-in 口能出网」仍属真机门。
fn select_fetch_client(state: &AppRuntime, want_proxy: bool) -> (Arc<HttpRuntime>, bool) {
    if want_proxy {
        let st = state.proxy().status();
        // 改用 update_in_port（此前用 mixed_port，随主流量策略）：同仓 icon_cache 已用 update-in 口。
        if st.running && st.update_in_port != 0 {
            if let Ok(c) = HttpRuntime::via_local_socks_proxy(st.update_in_port) {
                return (Arc::new(c), true);
            }
        }
    }
    (state.http().clone(), false)
}

/// SubscriptionFetchError → 前端分类错误 Value（`{ok:false, errorKind, message, httpStatus?}`）。
fn classify_fetch_error(e: &polaris_net_stack::subscription::SubscriptionFetchError) -> Value {
    // SubscriptionErrorKind serde 小写字面量 = TS errorKind，逐字对齐。
    let kind = serde_json::to_value(e.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let mut out = json!({ "ok": false, "errorKind": kind, "message": e.message });
    if let Some(status) = e.http_status {
        out["httpStatus"] = json!(status);
    }
    out
}

/// provider 子拉取失败的**永久性分类**（`SubscriptionFetchError` → [`ProviderFetchError`]）。
///
/// 判据 = 「重试还会不会变好」，不是「错得严不严重」：
/// - **permanent**：4xx（404 删了 / 403 撤权 / 410 gone）、SSRF guard 拒绝、URL 非法/协议不支持
///   —— 这些下一轮、下一天都还是一样。
/// - **transient**：超时、连不上、5xx、429（限流，等等就好）、正文解析失败（可能是 WAF 错误页）。
///
/// 为什么分类必须在这一层：`SubscriptionErrorKind` 与 `http_status` 只有拉取层有；net-stack 的编排
/// 函数只拿到一个 `String`，在那里靠子串猜状态码是必然漂移的假判据。分类结果决定
/// **该 provider 名下的存量节点本轮删不删**（见 [`ProviderFetchError`] 文档）。
fn classify_provider_fetch_error(
    e: &polaris_net_stack::subscription::SubscriptionFetchError,
) -> ProviderFetchError {
    // 429 = 限流：状态码在 4xx 段但语义是「稍后再来」→ 归 transient（否则一次限流就删光节点）。
    let permanent_status = matches!(e.http_status, Some(s) if (400..500).contains(&s) && s != 429);
    // SSRF guard 拒绝（内网/回环/重定向超限）与非 http(s) 协议：URL 本身的问题，重试不转好。
    let permanent_kind = matches!(
        e.kind,
        SubscriptionErrorKind::Ssrf | SubscriptionErrorKind::Scheme
    );
    if permanent_status || permanent_kind {
        ProviderFetchError::permanent(e.message.clone())
    } else {
        ProviderFetchError::transient(e.message.clone())
    }
}

// ── 订阅更新进度（`EVENT_SUBSCRIPTION_UPDATE_PROGRESS`）──────────────────────────
//
// 形态判据（为什么是**阶段名 + provider 计数**，不是百分比）：
//
// 百分比要求「已收字节 / 总字节」。总字节这一半是有的（`content-length` 响应头在
// `MinimalResponse::headers` 里，体积闸 `subscription.rs:421` 正在读它）——**分子这一半结构上不存在**：
// `HttpClient::fetch` 返回的是**已缓冲完的 `MinimalResponse.body: Vec<u8>`**（SSRF/重定向/体积三道
// guard 都建立在「整体收完再判」上），调用方拿到它时下载已经结束，中途没有任何可数的字节流。
// 这与同仓 `commands/rules.rs::download_with_progress` 判 `percent: null` 是**同一条**结论
// （那里逐字写着「宁可没有进度条，也不编一个匀速爬升的假条」），本处照同一口径。
//
// 且即便把传输层改成流式，百分比对本场景仍是错的呈现：订阅正文典型几十 KB，耗时几乎全在 TTFB
// （DNS + TLS + 机场服务端现场生成配置），进度条会在 0% 上冻十几秒再瞬间到 100%——比没有更误导。
//
// 真正有量的地方只有一处：Clash `proxy-providers` 的**逐个子拉取**（串行、可数、每个最长 15s），
// 那里给 `done/total` 真计数。其余阶段给阶段名。

/// 进度落点（注入式：生产广播给渲染端，单测用记录器 → 帧序可逐条对账）。
///
/// 抽 trait 而不是直接在发射点写 `broadcast(app, …)`：本仓未引 `tauri::test`，没有 `AppHandle`
/// 就无法在单测里证伪任何一帧。同 `commands/rules.rs::ProgressSink` 的先例。
pub(crate) trait UpdateProgressSink: Send + Sync {
    fn emit(&self, frame: Value);
}

/// 生产落点：补上 `subscriptionId` 后广播给全部窗口。
struct BroadcastUpdateProgress {
    app: AppHandle,
    subscription_id: String,
}

impl UpdateProgressSink for BroadcastUpdateProgress {
    fn emit(&self, mut frame: Value) {
        if let Some(obj) = frame.as_object_mut() {
            obj.insert("subscriptionId".to_string(), json!(self.subscription_id));
        }
        broadcast(&self.app, EVENT_SUBSCRIPTION_UPDATE_PROGRESS, frame);
    }
}

/// provider 逐个拉取的计数器。
///
/// `done` 语义 = **已完成数**，实现方式是在每次子拉取**发起前**取「已发起数」——两者相等的前提是
/// `resolve_proxy_providers` **串行**执行（它的文档写明「声明序，顺序执行」，根因是 `&mut id_gen`
/// 不可跨并发共享）。若哪天那里改成并发，本计数会变成「已发起数」而偏大；那是显示偏差不是正确性
/// 问题，但改动方需要回来改这里的语义。
///
/// `total` 取 `min(声明数, max_providers)` = 实际会被拉的上界。**上界而非精确值**：条目自身
/// `type != http` 或缺 `url` 会被 `resolve_proxy_providers` 跳过、根本不调本闭包，故收尾时
/// `done` 可能停在 `total` 之下。如实报上界优于伪造一个「刚好走满」的分母。
struct ProviderProgress {
    sink: Arc<dyn UpdateProgressSink>,
    total: usize,
    started: std::sync::atomic::AtomicUsize,
}

impl ProviderProgress {
    fn new(sink: Arc<dyn UpdateProgressSink>, total: usize) -> Self {
        Self {
            sink,
            total,
            started: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 一次子拉取即将发起 → 发一帧（首帧 `0/n`：此刻确实一个都还没完成）。
    fn on_fetch_start(&self) {
        let done = self
            .started
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sink.emit(json!({
            "phase": "providers",
            "done": done,
            "total": self.total,
        }));
    }
}

/// 由 [`perform_subscription_update_inner`] 的返回值派生**终态帧**（纯函数）。
///
/// 输入就是前端 `updateServers` 拿到的那个业务结果 —— 终态帧与它同源，故不可能出现
/// 「toast 说成功、订阅栏挂着失败」这种两个真值源打架的形态。
fn terminal_progress_frame(result: &Value) -> Value {
    if result.get("success").and_then(Value::as_bool) != Some(true) {
        return json!({
            "phase": "failed",
            "error": result.get("error").and_then(Value::as_str).unwrap_or_default(),
        });
    }
    if result.get("unchanged").and_then(Value::as_bool) == Some(true) {
        return json!({ "phase": "unchanged" });
    }
    let count = |k: &str| result.get(k).and_then(Value::as_u64).unwrap_or(0);
    json!({
        "phase": "done",
        "added": count("addedServers"),
        "updated": count("updatedServers"),
        "deleted": count("deletedServers"),
    })
}

/// provider 子拉取闭包（'static：owns Arc<client> + ua，复用同一 SSRF-guarded 拉取路径）。
fn build_provider_fetch(
    client: Arc<HttpRuntime>,
    ua: String,
    via_proxy: bool,
    progress: Option<Arc<ProviderProgress>>,
) -> impl Fn(
    &str,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<String, ProviderFetchError>> + Send>,
> + Send
       + Sync {
    move |u: &str| {
        let client = client.clone();
        let ua = ua.clone();
        let u = u.to_string();
        // 闭包体是**同步**跑的（返回 future 之前）⇒ 计数帧在子拉取真正开始前就发出去，
        // 不必把计数器搬进 future 里。preview 腿传 None ⇒ 一帧不发。
        if let Some(p) = &progress {
            p.on_fetch_start();
        }
        Box::pin(async move {
            // provider 拉取用较紧超时（PROVIDER_FETCH_TIMEOUT_MS）；DNS 用 SystemDnsLookup（provider 属
            // 生产 Clash 订阅，生产主拉取的 lookup 亦 SystemDnsLookup，二者一致）。
            fetch_subscription_full(
                client.as_ref(),
                &SystemDnsLookup,
                &u,
                &ua,
                None,
                via_proxy,
                PROVIDER_FETCH_TIMEOUT_MS,
            )
            .await
            .map_err(|e| classify_provider_fetch_error(&e))
        })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, ProviderFetchError>> + Send>,
            >
    }
}

/// 拉取主订阅（+元数据/条件 GET）→ 解析 → proxy-providers 编排合并去重。**生产与门共用**。
///
/// `Err(Value)` = 分类错误。0 节点**不在此判错**（交调用方：preview 报 empty；update 走 merge-only）。
/// 304 → `not_modified=true`、servers 空、回传验证器。
///
/// `progress`：provider 逐个拉取的计数落点。**update 腿传 `Some`、preview 腿传 `None`** ——
/// 预检发生在「新增订阅」对话框里，此刻还没有 `subscriptionId`（传的是空串），往订阅信息栏推的
/// 帧会没有归属；且那条路已有自己的对话框内反馈。
// 8 个参数：同 `net-stack::subscription` 那两条拉取入口的处置（各自 `#[allow]`）。这些参数全是
// 独立的拉取旋钮，打包成 struct 只会在两个调用点各多一层字段名，不减少任何需要理解的东西。
#[allow(clippy::too_many_arguments)]
async fn fetch_parse_resolve<L: DnsLookup>(
    client: Arc<HttpRuntime>,
    lookup: &L,
    url: &str,
    subscription_id: &str,
    via_proxy: bool,
    user_agent: Option<&str>,
    conditional: Option<&Conditional>,
    progress: Option<&Arc<dyn UpdateProgressSink>>,
) -> Result<FetchOutcome, Value> {
    let ua = user_agent
        .map(str::to_string)
        .unwrap_or_else(|| default_subscription_user_agent(env!("CARGO_PKG_VERSION")));
    let fetched = fetch_subscription_with_meta(
        client.as_ref(),
        lookup,
        url,
        &ua,
        conditional,
        via_proxy,
        MAIN_FETCH_TIMEOUT_MS,
    )
    .await
    .map_err(|e| classify_fetch_error(&e))?;

    // 304 → 短路（不 parse/reconcile）；回传验证器供刷新。
    if fetched.not_modified {
        return Ok(FetchOutcome {
            not_modified: true,
            etag: fetched.etag,
            last_modified: fetched.last_modified,
            ..Default::default()
        });
    }

    let now = current_iso();
    let mut id_gen = new_uuid;
    // `RemoteSubscription`：正文来自网络 ⇒ 未建模 type **不**透传 custom。custom 逃生舱把原始
    // outbound JSON 逐字下发内核，而内核的 `tor` outbound 收 `executable_path`/`extra_args`
    // （实测 `sing-box check` rc=0）⇒ 那条腿对远端订阅等于任意本机命令执行。见 [`ImportOrigin`]。
    let mut parsed = parse_subscription(
        &fetched.text,
        subscription_id,
        &now,
        &mut id_gen,
        ImportOrigin::RemoteSubscription,
    );
    let mut partial = false;

    // proxy-providers 编排（Clash provider 型订阅）：拉各 provider → 合并 + 同指纹去重（内联优先）。
    let mut failed_providers = Vec::new();
    let has_providers = if let Some(providers) = extract_proxy_providers(&fetched.text) {
        // 分母取「声明数 ∩ 上限」= 实际会被拉的上界（见 `ProviderProgress` 文档的上界说明）。
        let declared = providers.as_mapping().map_or(0, |m| m.len());
        let counter = progress.map(|sink| {
            Arc::new(ProviderProgress::new(
                Arc::clone(sink),
                declared.min(default_max_providers()),
            ))
        });
        let fetch = build_provider_fetch(client.clone(), ua.clone(), via_proxy, counter);
        let pres = resolve_proxy_providers(
            &providers,
            subscription_id,
            &now,
            default_max_providers(),
            &fetch,
            &mut id_gen,
        )
        .await;
        partial = pres.any_failed;
        // provider 级精确 merge-back 所需（见 reconcile 的 `failed_providers`）：此前只取
        // `any_failed`、丢掉这份名单 → 整订阅 merge_only、`deleted` 恒 0 → 成功 provider 名下的
        // 真下架节点也永久滞留。
        failed_providers = pres.failed_providers;
        parsed.warnings.extend(pres.warnings);
        let mut merged = parsed.servers;
        merged.extend(pres.servers);
        parsed.servers = dedupe_by_fingerprint(merged);
        true
    } else {
        // 兜底：JSON/YAML 两种编码的 `proxy-providers` 现均由 `extract_proxy_providers` 覆盖
        // （detect_format 单一真值），到不了这里；仍保留串探测作纵深防御——误判只会**多**豁免一次
        // 条件 GET（保守方向），漏判则会让主正文 304 掩盖 provider 变化（危险方向）。
        fetched.text.contains("\"proxy-providers\"")
    };

    Ok(FetchOutcome {
        servers: parsed.servers,
        warnings: parsed.warnings,
        user_info: fetched.user_info.map(|u| u.to_json()),
        etag: fetched.etag,
        last_modified: fetched.last_modified,
        not_modified: false,
        partial,
        failed_providers,
        has_providers,
    })
}

/// 拉取+解析+provider 编排产出（update / preview 共用）。
#[derive(Default)]
struct FetchOutcome {
    servers: Vec<ServerConfig>,
    warnings: Vec<String>,
    /// `Subscription-UserInfo` 流量/到期（前端 `userInfo` json 形态）。
    user_info: Option<Value>,
    etag: Option<String>,
    last_modified: Option<String>,
    not_modified: bool,
    /// provider transient 失败 → reconcile 改 merge-only 防穿仓。
    partial: bool,
    /// transient 失败的 provider 名单（provider 级精确 merge-back：只保留**失败** provider 名下的
    /// 下架节点，成功 provider 的正常删除）。空 = 失败名未知 → 退回整订阅级 merge-only。
    failed_providers: Vec<String>,
    /// 含 Clash proxy-providers（回写 sub → 下次豁免条件 GET）。
    has_providers: bool,
}

/// 单订阅拉取 + 对账 + 落盘 + 元数据回写（items 2/3/6/7）。**命令与 scheduler 共用**。
///
/// 返回业务结果 Value（`{success, addedServers, updatedServers, deletedServers, unchanged?, userInfo?}`
/// 或 `{success:false, error}`）；信封由调用方包 ok（前端读 `data.success`）。
///
/// 本函数是**薄壳**：只负责「起手一帧 + 终态一帧」，真实现全在
/// [`perform_subscription_update_inner`]。这样拆的唯一理由是**终态必达**——内层有七八条
/// `return update_failure(...)` 早退，逐条在前面补一句 emit 是能写对，但下一个人新加一条早退时
/// 不会想起来补，订阅栏就会永远挂在「更新中」。派生自返回值 ⇒ 结构上漏不掉。
pub(crate) async fn perform_subscription_update(
    app: &AppHandle,
    state: &AppRuntime,
    subscription_id: &str,
) -> Value {
    let sink: Arc<dyn UpdateProgressSink> = Arc::new(BroadcastUpdateProgress {
        app: app.clone(),
        subscription_id: subscription_id.to_string(),
    });
    // 起手即报「拉取中」：这一帧之前的活（load_full / UA 求值）是微秒级，不值一个独立阶段。
    sink.emit(json!({ "phase": "fetching" }));
    let result = perform_subscription_update_inner(app, state, subscription_id, &sink).await;
    sink.emit(terminal_progress_frame(&result));
    result
}

/// 真实现。**任何早退都不必自己发终态帧**——外壳按返回值派生（见
/// [`perform_subscription_update`] 文档）。
async fn perform_subscription_update_inner(
    app: &AppHandle,
    state: &AppRuntime,
    subscription_id: &str,
    sink: &Arc<dyn UpdateProgressSink>,
) -> Value {
    // 1) 取订阅元数据（URL + per-sub UA/viaProxy + 条件 GET 验证器）。
    let cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return update_failure(format!("{e}")),
    };
    let Some(sub) = find_subscription(&cfg, subscription_id) else {
        return update_failure(format!("订阅不存在: {subscription_id}"));
    };
    let url = match sub
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(u) => u.to_string(),
        None => return update_failure("订阅缺少 URL"),
    };
    let user_agent = resolve_subscription_ua(&cfg, &sub);
    let want_proxy = want_proxy_for_sub(&cfg, &sub);
    // 条件 GET：provider 型订阅豁免（主正文 304 掩盖 provider 独立变化）；否则带上次验证器。
    let conditional = if sub.get("hasProviders").and_then(Value::as_bool) == Some(true) {
        None
    } else {
        Some(Conditional {
            etag: sub.get("etag").and_then(Value::as_str).map(str::to_string),
            last_modified: sub
                .get("lastModified")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    };

    // 2) 拉取 + 解析 + provider 编排。
    let (client, via_effective) = select_fetch_client(state, want_proxy);

    // 2a) **显式强制经代理却经不了 → fail-closed，绝不静默直连**。
    //
    // `subscriptionProxyPolicy="proxy"` 是用户在设置页显式勾的全局强制，语义 = 「订阅地址不许
    // 明文出网」。核未跑 / `update-in` 口为 0 时静默回退直连，会把订阅 URL 的 DNS 查询与 TLS SNI
    // 直接暴露给本地网络 —— 而且 `result` 里没有任何一个字段能让用户看出这次是明文拉的
    // （`{success:true, addedServers:…}` 与经代理成功**逐字无别**）。这不是「降级 UX」，是把用户
    // 明确要求避开的那件事悄悄做了。
    //
    // 为什么这里与 上游 分叉（**刻意**）：上游的回退发生在它只有 per-sub `viaProxy` 偏好的路径上
    // （`SubscriptionService:690-698`），那是「偏好落空 → 退直连」，合理；本仓的三态策略多出一个
    // **显式强制**档，对它照抄回退就是把「强制」实现成了「建议」。`follow` 档（含 per-sub
    // `updateViaProxy=true`）**保持**静默回退不变，自举友好不受影响。
    if !via_effective && proxy_policy_is_forced(&cfg) {
        let st = state.proxy().status();
        log::warn!(
            "订阅 {subscription_id} 更新中止：全局策略强制经代理，但代理不可用\
             （running={}, update_in_port={}）——不静默直连，以免订阅地址明文外泄",
            st.running,
            st.update_in_port
        );
        return update_failure(
            "全局策略要求「所有订阅经代理更新」，但当前代理不可用（未运行或 update-in 端口未分配）。\
             已中止本次更新以避免订阅地址明文外泄；请先启动代理，或把订阅代理策略改为「跟随」/「直连」。",
        );
    }

    let lookup = SystemDnsLookup;
    let outcome = match fetch_parse_resolve(
        client,
        &lookup,
        &url,
        subscription_id,
        via_effective,
        user_agent.as_deref(),
        conditional.as_ref(),
        Some(sink),
    )
    .await
    {
        Ok(o) => o,
        Err(err_value) => {
            let msg = err_value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("订阅更新失败")
                .to_string();
            return update_failure(msg);
        }
    };

    // 3) 304 无变化 → 仅刷元数据（lastUpdated + 验证器），不 reconcile、不广播（零节点扰动、不断流）。
    if outcome.not_modified {
        let mut cfg = match state.config().load_full() {
            Ok(c) => c,
            Err(e) => return update_failure(format!("{e}")),
        };
        if let Some(s) = find_subscription_mut(&mut cfg, subscription_id) {
            s["lastUpdated"] = json!(current_iso());
            if let Some(e) = &outcome.etag {
                s["etag"] = json!(e);
            }
            if let Some(lm) = &outcome.last_modified {
                s["lastModified"] = json!(lm);
            }
        }
        let _ = state.config().save_full(&cfg); // 落盘验证器，**不广播**（不触发 switch_mode 断流）
        return update_ok(0, 0, 0, true, None);
    }

    // 4) 空集（非 partial）→ merge-only 失败：0 节点极可能拉取半通/解析失败，permanent 删不可逆。
    if outcome.servers.is_empty() && !outcome.partial {
        let detail = outcome
            .warnings
            .first()
            .cloned()
            .unwrap_or_else(|| "解析得到 0 个可用节点".to_string());
        return update_failure(detail);
    }

    // 5) reconcile（最新 config；partial → merge_only 防穿仓）。
    //
    // 网络腿到此为止，余下是对账 + 落盘 + 广播（广播可能触发内核热切换/重启）。单独报一个阶段是因为
    // 它与「拉取中」的失败含义完全不同：卡在这里是本地磁盘/配置问题，不是机场不通。
    sink.emit(json!({ "phase": "applying" }));
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return update_failure(format!("{e}")),
    };
    let old_selected = cfg
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let user_info = outcome.user_info.clone();
    let recon = reconcile_subscription_servers(
        &mut cfg,
        subscription_id,
        outcome.servers,
        outcome.partial,
        &outcome.failed_providers,
    );
    let content_changed = recon.added > 0 || recon.updated > 0 || recon.deleted > 0;

    // 6) 元数据回写到 sub 记录（lastUpdated + userInfo + 验证器 + hasProviders）。
    write_sub_metadata(
        &mut cfg,
        subscription_id,
        user_info.as_ref(),
        outcome.etag.as_deref(),
        outcome.last_modified.as_deref(),
        outcome.has_providers,
    );

    // 7) L-5：200 但节点集内容等价（!content_changed && !partial）→ 仅刷元数据 return unchanged，
    //    **不广播**（避免字节级无变化也 switch_mode 断流）。
    if !content_changed && !outcome.partial {
        let _ = state.config().save_full(&cfg);
        return update_ok(0, 0, 0, true, user_info.as_ref());
    }

    // 8) 内容变 / partial → 落盘 + 广播（汇流点自动热切换）+ 出口变则作废解锁缓存。
    if let Err(e) = state.config().save_full(&cfg) {
        return update_failure(format!("{e}"));
    }
    broadcast_config_changed(app, &cfg);
    if selected_exit_changed(
        old_selected.as_deref(),
        cfg.get("selectedServerId").and_then(Value::as_str),
    ) {
        let sink = BroadcastSink::new(app);
        let running = state.proxy().status().running;
        state.unlock().invalidate(&sink, running, false);
    }
    update_ok(
        recon.added,
        recon.updated,
        recon.deleted,
        false,
        user_info.as_ref(),
    )
}

/// 按 id 取订阅记录（只读克隆）。
fn find_subscription(cfg: &Value, id: &str) -> Option<Value> {
    cfg.get("subscriptions")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
        })
        .cloned()
}

/// 按 id 取订阅记录可变引用（元数据回写）。
fn find_subscription_mut<'a>(cfg: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    cfg.get_mut("subscriptions")
        .and_then(Value::as_array_mut)?
        .iter_mut()
        .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
}

/// 200 路径回写 sub 元数据：lastUpdated + userInfo（有则写）+ etag/lastModified（**无条件**，L-3：
/// 服务端撤 validator 后不残留旧值致伪 304）+ hasProviders。
fn write_sub_metadata(
    cfg: &mut Value,
    id: &str,
    user_info: Option<&Value>,
    etag: Option<&str>,
    last_modified: Option<&str>,
    has_providers: bool,
) {
    let Some(s) = find_subscription_mut(cfg, id) else {
        return;
    };
    s["lastUpdated"] = json!(current_iso());
    if let Some(ui) = user_info {
        s["userInfo"] = ui.clone();
    }
    set_or_remove(s, "etag", etag);
    set_or_remove(s, "lastModified", last_modified);
    s["hasProviders"] = json!(has_providers);
}

/// 置键（`Some`）或删键（`None`）——L-3 无条件写验证器（含清除）。
fn set_or_remove(obj: &mut Value, key: &str, val: Option<&str>) {
    if let Some(o) = obj.as_object_mut() {
        match val {
            Some(v) => {
                o.insert(key.to_string(), json!(v));
            }
            None => {
                o.remove(key);
            }
        }
    }
}

/// 订阅节点对账结果计数。
struct ReconcileOutcome {
    added: usize,
    updated: usize,
    deleted: usize,
}

/// 节点稳定指纹（对账键）：`protocol|address|port|cred|network`（**排除 name**）。
/// 上游 `SubscriptionService.serverFingerprint`。同一物理节点跨拉取指纹一致 → reconcile 命中。
///
/// 排除 name：订阅方常改名/调顺序，用 name 做键会把同一物理节点误判「删旧增新」→ id 抖动、
/// selectedServerId 丢失、本地编辑被清。cred（uuid / password / 嵌套 ss·ssh password / username /
/// wg peerPublicKey）区分同 host:port 并列节点；network 维度区分同 host:port:cred 但传输不同的节点。
///
/// **与 net-stack `server_fingerprint(&ServerConfig)` 是同一公式的 json 侧**——作用于 `Value`（对账两侧
/// 均为 camelCase JSON），由 [`reconcile_tests::fingerprint_matches_net_stack_typed`] 跨类型等价单测锁定。
fn node_fingerprint(v: &Value) -> String {
    let protocol = v
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let address = v.get("address").and_then(Value::as_str).unwrap_or("");
    let port = v.get("port").and_then(Value::as_u64).unwrap_or(0);
    let network = v
        .get("network")
        .and_then(Value::as_str)
        .unwrap_or("tcp")
        .to_ascii_lowercase();
    let cred = node_cred(v);
    format!("{protocol}|{address}|{port}|{cred}|{network}")
}

/// 凭据落点（上游 `cred` 链）：uuid → password → shadowsocksSettings.password → username →
/// sshSettings.password → wireguardSettings.peerPublicKey，首个非空即取（空串视缺）。
fn node_cred(v: &Value) -> String {
    let pick = |val: Option<&Value>| {
        val.and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    pick(v.get("uuid"))
        .or_else(|| pick(v.get("password")))
        .or_else(|| pick(v.get("shadowsocksSettings").and_then(|s| s.get("password"))))
        .or_else(|| pick(v.get("username")))
        .or_else(|| pick(v.get("sshSettings").and_then(|s| s.get("password"))))
        .or_else(|| {
            pick(
                v.get("wireguardSettings")
                    .and_then(|s| s.get("peerPublicKey")),
            )
        })
        .unwrap_or_default()
}

/// 兜底出口候选选择（上游 `pickViableFallbackExit`，main 侧无 latency → 取首个可用候选）。
///
/// 候选须过可用性谓词——排 subnet-only 组网节点（WG allowInternet:false 带网段 / TS 无 exitNode），
/// 否则重启后公网流量静默走 direct = VPN 语义泄漏（#291）。返回首个「可反序列化 + 承载全隧道 + 可路由」
/// 节点 id；全不可用 → `None`（调用方置 `DIRECT_SERVER_ID` 哨兵 = 显式可见直连，非裸 null）。
///
/// **注**：完整 `isServerComplete` 字段校验属 nodes-servers 域（另有专项）；此处用 config-engine 既有
/// `mesh_node_carries_full_tunnel` + `!is_mesh_node_unroutable` 覆盖 #291 泄漏面（组网节点承载性），
/// 结构完整性以「可反序列化为 ServerConfig」近似。
fn pick_viable_fallback_exit(servers: &[Value]) -> Option<String> {
    for s in servers {
        let Some(id) = s
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty())
        else {
            continue;
        };
        // 结构完整（可反序列化）+ 非 unroutable + 承载全隧道 → 可作兜底出口。
        if let Ok(sc) = serde_json::from_value::<ServerConfig>(s.clone()) {
            if !is_mesh_node_unroutable(&sc) && mesh_node_carries_full_tunnel(&sc) {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// partial（provider 瞬时失败）时，这个「已下架」的存量节点是否**保留**（provider 级精确 merge-back）。
/// 上游 `SubscriptionService.leftoverToKeep` 1:1。
///
/// 三条规则，顺序即优先级：
/// 1. `failed_providers` 为空 —— 失败 provider 名未知（整订阅级失败兜底）→ **全保留**，退回旧的整订阅
///    merge-only（宁滞留不误删）；
/// 2. 节点无 `providerName`（迁移前存量 / 主正文内联 `proxies` / 非 Clash 订阅）—— 没有归属信息可判，
///    **保守保留**；
/// 3. 否则仅保留**失败** provider 名下的 —— 成功 provider 名下的下架是真下架，正常删除。
///
/// 规则 3 是本函数存在的理由：此前 partial 一律整订阅 merge-only，某个 provider 503 会连带让**成功**
/// provider 名下的真下架节点无限滞留在列表里（且 `deletedServers` 恒 0，前端看不出）。
fn leftover_survives_partial(node: &Value, failed_providers: &[String]) -> bool {
    if failed_providers.is_empty() {
        return true;
    }
    match node
        .get("providerName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        None => true,
        Some(name) => failed_providers.iter().any(|f| f == name),
    }
}

/// 剥离易变元数据（id/时间戳）后比较，判「内容是否真变了」（避免仅 id/时间不同就误报 updated）。
fn strip_volatile(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("id");
        o.remove("createdAt");
        o.remove("updatedAt");
    }
    v
}

/// 破坏性对账：以拉取的新节点集为准，对本订阅节点做差集（add/update/remove），
/// **他订阅节点与自建节点（subscriptionId 不匹配）一律不动**。
///
/// 命中（指纹一致）：保留原 `id` + `createdAt`（选中节点 id 不失效、用户可见身份不变），
/// 其余字段以新拉取为准。选中节点被删 → `selectedServerId` 置 `pick_viable_fallback_exit ?? DIRECT`
/// 哨兵（**绝不裸 null**，避 generate 回归 + 不静默泄漏，对齐 上游 手动/自动路径 F14 reselect）。
///
/// `merge_only=true`（provider transient 失败）→ **provider 级精确** merge-back（上游
/// `SubscriptionService.leftoverToKeep`，见 [`leftover_survives_partial`]）：只保留「失败 provider 名下 +
/// 无归属」的下架节点，**成功 provider 名下的真下架节点照常删除**。`failed_providers` 为空（失败名未知）
/// → 退回旧的整订阅级 merge-only（全保留、`deleted=0`）。被保留的节点不计入 `deleted`。
fn reconcile_subscription_servers(
    cfg: &mut Value,
    sub_id: &str,
    new_servers: Vec<ServerConfig>,
    merge_only: bool,
    failed_providers: &[String],
) -> ReconcileOutcome {
    use std::collections::{HashMap, VecDeque};

    let new_vals: Vec<Value> = new_servers
        .iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .collect();

    let existing: Vec<Value> = cfg
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 分区：本订阅节点 vs 其他（其他原样保留）。
    let (this_sub, mut others): (Vec<Value>, Vec<Value>) = existing
        .into_iter()
        .partition(|s| s.get("subscriptionId").and_then(Value::as_str) == Some(sub_id));

    // 指纹 → 该指纹下**所有**现有节点的 FIFO 队列。指纹（protocol/address/port/cred/network，**不含 name**）
    // 会碰撞：同 uuid 同 host:port 同传输、仅 name/SNI/path 异的 CDN/中转节点常见。此前用 `insert` 只留最后一个
    // → 丢其余 id；且两个同指纹新节点会 `get` 到同一个旧节点 → 复制**同一个** id（重复 id）。
    // 改为保留全部 + 逐个 1:1 消费（`pop_front`），彻底杜绝 id 丢失与重复。
    let mut existing_by_key: HashMap<String, VecDeque<Value>> = HashMap::new();
    for s in &this_sub {
        existing_by_key
            .entry(node_fingerprint(s))
            .or_default()
            .push_back(s.clone());
    }

    let mut reconciled: Vec<Value> = Vec::with_capacity(new_vals.len());
    let (mut added, mut updated) = (0usize, 0usize);
    for mut nv in new_vals {
        let key = node_fingerprint(&nv);
        // 1:1 消费：pop 掉一个已匹配的旧节点，防第二个同指纹新节点复用同一 id。
        let matched = existing_by_key.get_mut(&key).and_then(VecDeque::pop_front);
        if let Some(old) = matched {
            // 命中 → 保留稳定 id + 原 createdAt。
            if let Some(nobj) = nv.as_object_mut() {
                if let Some(oid) = old.get("id").cloned() {
                    nobj.insert("id".to_string(), oid);
                }
                if let Some(created) = old.get("createdAt").cloned() {
                    nobj.insert("createdAt".to_string(), created);
                }
            }
            if strip_volatile(&nv) != strip_volatile(&old) {
                updated += 1;
            }
        } else {
            added += 1;
        }
        reconciled.push(nv);
    }

    // 未被任何新节点 1:1 消费掉的现有本订阅节点（队列剩余）。指纹整体消失 + 碰撞未配对都算。
    let leftover: Vec<Value> = existing_by_key.into_values().flatten().collect();
    let leftover_total = leftover.len();
    // merge_only（provider 部分失败）→ 按 provider 精确挑保留项；否则 leftover 整体即删除集。
    let kept: Vec<Value> = if merge_only {
        leftover
            .into_iter()
            .filter(|s| leftover_survives_partial(s, failed_providers))
            .collect()
    } else {
        Vec::new()
    };
    // 报给前端的 deleted = **实际**删掉的（= leftover 总数 − 被 merge-back 保留的）。
    // 此前 merge_only 恒 0，把「成功 provider 下架了 3 个节点」谎报成「无变化」。
    let deleted = leftover_total.saturating_sub(kept.len());

    others.extend(reconciled);
    others.extend(kept); // provider 级 merge-back：失败 provider / 无归属的存量保留
    if let Some(o) = cfg.as_object_mut() {
        o.insert("servers".to_string(), Value::Array(others));
    }

    // 悬挂 selectedServerId：按**实际结果 id 集**校验，仅当选中 id 真不在最终 servers 才兜底。
    // 此前按「指纹缺失」判：碰撞丢 id 时选中项指纹仍在却指向已消失 id → 漏判、留悬空引用。直连哨兵
    // `__direct__` 与空值不是节点 id，不参与存在性校验（否则会被误兜底）。
    let selected = cfg
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(sid) = selected {
        if !sid.is_empty() && !is_direct_selection(Some(&sid)) {
            let servers = cfg
                .get("servers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let still_exists = servers
                .iter()
                .any(|s| s.get("id").and_then(Value::as_str) == Some(sid.as_str()));
            if !still_exists {
                // F14 逃死节点：选可用兜底出口逃离已下架的选中节点；全不可用 → direct 哨兵。
                // **绝不裸 null**（避 generate `Selected server not found` 回归 + 不静默泄漏）。
                let fallback = pick_viable_fallback_exit(&servers)
                    .unwrap_or_else(|| DIRECT_SERVER_ID.to_string());
                if let Some(o) = cfg.as_object_mut() {
                    o.insert("selectedServerId".to_string(), json!(fallback));
                }
            }
        }
    }

    ReconcileOutcome {
        added,
        updated,
        deleted,
    }
}

/// 订阅预检核心（**生产与门共用同一份**）：拉取 → 解析 → 节点计数，产出前端 `SubscriptionPreviewResult`。
///
/// 泛型注入 `client`/`lookup`：
/// - **生产**：`client = state.http()`（真 reqwest）、`lookup = SystemDnsLookup`（真系统解析）；
/// - **组合面门**：`client = 真 HttpRuntime`（resolve 钉到回环）、`lookup = 真 SystemDnsLookup`（解析真公网 hostname）。
///
/// §K7.1 纪律：门必须驱动**这个函数**（生产唯一路径），不得只测 mock HttpClient。
/// 复用 [`fetch_parse_resolve`]（含 provider 编排，与 updateServers 同一份）→ 节点计数（含 provider 节点）。
pub(crate) async fn preview_core<L: DnsLookup>(
    client: Arc<HttpRuntime>,
    lookup: &L,
    url: &str,
    via_proxy: bool,
    user_agent: Option<&str>,
) -> Value {
    // subscription_id 传空串（预检不建订阅记录）；conditional=None（无 sub 无验证器）；
    // progress=None（还没有订阅记录可归属，见 `fetch_parse_resolve` 的 `progress` 文档）。
    match fetch_parse_resolve(client, lookup, url, "", via_proxy, user_agent, None, None).await {
        Ok(outcome) => {
            // 解析：0 节点 → errorKind:'empty'（对齐 local_import_parse 的 0 节点不变式，防空集删存量）。
            let node_count = outcome.servers.len();
            if node_count == 0 {
                let detail = outcome
                    .warnings
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "解析得到 0 个可用节点".to_string());
                return json!({ "ok": false, "errorKind": "empty", "message": detail });
            }
            json!({ "ok": true, "nodeCount": node_count })
        }
        Err(err_value) => err_value,
    }
}

/// 上游 `SUBSCRIPTION_PREVIEW`：新增订阅前预检（拉取+解析 URL，不写 config）。
///
/// **已接线**（2026-07-16）：注入 `runtime/http.rs` 传输层单点 → `preview_core` → 拉取+解析。
/// 返回形态对齐前端 `SubscriptionPreviewResult`（`ui/src/shared/subscription-preview.ts`）。
///
/// `via_proxy=true`：经本机 **update-in** 端口的 client（订阅站被墙时经代理拉；隔离主流量策略）。
/// 核未起/端口未知则回落直连 client（如实：没代理可用就直连，而非假装经代理）。
#[tauri::command]
pub async fn subscription_preview(
    state: State<'_, AppRuntime>,
    url: String,
    via_proxy: Option<bool>,
    user_agent: Option<String>,
) -> Result<ApiResponse<Value>, ()> {
    let via = via_proxy.unwrap_or(false);
    let lookup = SystemDnsLookup;
    let (client, via_effective) = select_fetch_client(state.inner(), via);
    let result = preview_core(client, &lookup, &url, via_effective, user_agent.as_deref()).await;
    Ok(ApiResponse::ok(result))
}

/// 上游 `LOCAL_IMPORT_PARSE`：本地导入解析（文件/文本 → 节点预览，不联网）。
///
/// 接 net-stack [`parse_subscription`](polaris_net_stack::subscription::parse_subscription)
/// （纯逻辑，无 I/O）：Clash YAML / base64 / url-list 三路分发。产出对齐前端
/// `ImportParseResult`（`ui/src/shared/types.ts:254`）。
///
/// **本地导入的节点是「自建」节点** → 不带 `subscriptionId`（前端契约注释明示）：故 subscription_id
/// 传空串后剥离该字段。`subscriptions[]` 仅来自 Clash proxy-providers（不联网拉取），当前 net-stack
/// 的 provider 编排属拉取层 → 恒空，见下方注释。
///
/// 体积闸（10 MB，与 上游 `MAX_BODY_BYTES` 同口径）：防超大文件 / YAML 锚点炸弹经 IPC 进主进程 OOM。
#[tauri::command]
pub fn local_import_parse(_state: State<'_, AppRuntime>, text: String) -> ApiResponse<Value> {
    const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
    let trimmed = text.trim();
    if trimmed.len() > MAX_BODY_BYTES {
        return ApiResponse::err("导入内容过大");
    }

    let format = polaris_net_stack::subscription::detect_format(trimmed);
    let now = current_iso();
    let mut id_gen = new_uuid;
    // subscription_id 传空串：本地导入产出自建节点，下方剥离该字段（不归属任何订阅）。
    // `LocalFile`：内容是用户自己的文件/粘贴 ⇒ 未建模 type 透传为 custom 逃生舱
    // （上游 `parseLocalContent` 的 `makeCustomNode` 腿，见 [`ImportOrigin`]）。
    let parsed = polaris_net_stack::subscription::parse_subscription(
        trimmed,
        "",
        &now,
        &mut id_gen,
        ImportOrigin::LocalFile,
    );
    // 「不支持协议 → 透传为 custom」的条数。**由产物派生而非解析器另报一个计数**：契约
    // （`ui/src/contracts/types.ts` `ImportParseResult.stats.unsupported`）的定义就是
    // 「imported 里已透传为 custom 的数量」，派生使二者恒等而非靠两处各自加对；且解析管线里
    // 只有 sing-box 这条腿会产出 `Protocol::Custom`（clash / xray / share-link 三个解析器
    // grep 零命中）。
    let unsupported = parsed
        .servers
        .iter()
        .filter(|s| s.protocol == Protocol::Custom)
        .count();

    let nodes: Vec<Value> = parsed
        .servers
        .iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .map(|mut v| {
            if let Some(o) = v.as_object_mut() {
                o.remove("subscriptionId"); // 自建节点无归属
            }
            v
        })
        .collect();

    // 0 节点即报错（与 Polaris 对齐）：空集会让渲染端「导入成功但什么也没有」，
    // 且不可识别格式本就该由前端门控报错（ipc-channels.ts:31「不可识别格式 throw」）。
    if nodes.is_empty() {
        let detail = parsed
            .warnings
            .first()
            .cloned()
            .unwrap_or_else(|| "未识别到任何可用节点".to_string());
        return ApiResponse::err(format!("解析得到 0 个可用节点：{detail}"));
    }

    ApiResponse::ok(json!({
        "nodes": nodes,
        // 仅来自 Clash proxy-providers；provider 需联网拉取 → 本地导入路径恒空（对齐「不联网拉取」契约）。
        "subscriptions": [],
        "stats": {
            "imported": nodes.len(),
            // custom 透传：不建模的 sing-box type（`hysteria` v1 / `tor` / 独立 `shadowtls` /
            // `openconnect` / `openvpn-*` …）原文入库、可编辑，使用时由 `kernel:probeOutbound`
            // 按内核实况置灰（内核即权威，不在此复刻协议白名单）。
            "unsupported": unsupported,
            "skipped": parsed.skipped,
            "failed": parsed.failed,
        },
        "warnings": parsed.warnings,
        "format": import_format_label(format),
    }))
}

/// net-stack 格式探测 → 前端 `ImportFormat`（`ui/src/shared/types.ts:248`）。
///
/// 前端联合类型是 `'singbox'|'xray'|'clash'|'links'|'unknown'`：base64 与 url-list 在前端
/// 同属 `links`（两者只是编码差异，节点级语义一致）；`xray` 走 xray JSON 导入（C17 已移植）。
fn import_format_label(f: polaris_net_stack::subscription::SubscriptionFormat) -> &'static str {
    use polaris_net_stack::subscription::SubscriptionFormat as F;
    match f {
        F::Clash => "clash",
        F::SingboxJson => "singbox",
        F::XrayJson => "xray",
        F::Base64 | F::UrlList => "links",
        F::Unknown => "unknown",
    }
}

/// 上游 `LOCAL_IMPORT_PICK_FILE`：弹系统原生文件框 + 读内容回传（Tauri dialog 插件）。
///
/// 替代 Electron `dialog.showOpenDialog`（上游 `subscription-handlers.ts` `LOCAL_IMPORT_PICK_FILE`）：
/// 文件类型过滤对齐 上游（配置文件 json/yaml/yml/txt/conf + 所有文件）；10MB 上限对齐 Polaris
/// （与 [`local_import_parse`] 同口径）。回调式 `pick_file` + oneshot（对齐 `misc.rs::ask_open_path`）——
/// `blocking_pick_file` 在主线程调用会死锁，故本 command 为 `async fn`。
/// 取消 → `{canceled:true}`；文件过大/读失败 → `{canceled:false,error}`（`too_large`|`read_failed`，
/// 对齐 上游 错误码）；成功 → `{canceled:false,content,fileName}`（`fileName` 为 basename，非全路径，
/// 避免向渲染端泄漏本机目录结构）。
#[tauri::command]
pub async fn local_import_pick_file(window: WebviewWindow) -> ApiResponse<Value> {
    const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

    let lang = crate::i18n::app_lang(window.app_handle());
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_title(t(lang, key::NATIVE_CONFIG_PICK_TITLE))
        .add_filter(
            t(lang, key::NATIVE_CONFIG_FILE_TYPE),
            &["json", "yaml", "yml", "txt", "conf"],
        )
        .add_filter(t(lang, key::NATIVE_ALL_FILES), &["*"])
        .pick_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(path) = rx.await.ok().flatten().and_then(|p| p.into_path().ok()) else {
        return ApiResponse::ok(json!({ "canceled": true }));
    };

    match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > MAX_BODY_BYTES => {
            return ApiResponse::ok(json!({ "canceled": false, "error": "too_large" }));
        }
        Err(_) => return ApiResponse::ok(json!({ "canceled": false, "error": "read_failed" })),
        _ => {}
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ApiResponse::ok(json!({
                "canceled": false,
                "content": content,
                "fileName": file_name,
            }))
        }
        Err(_) => ApiResponse::ok(json!({ "canceled": false, "error": "read_failed" })),
    }
}

fn current_iso() -> String {
    // 复用 stats-engine 的 created_at_to_rfc3339（无 chrono/time 依赖）；旧实现把整个 epoch 秒塞进秒字段
    // 产出非法 ISO（前端 Invalid Date），改为真 epoch-millis→RFC3339。时钟异常 → 空串，不 panic。
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .and_then(polaris_stats_engine::created_at_to_rfc3339)
        .unwrap_or_default()
}

fn new_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pol-{nanos:032x}")
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use polaris_config_engine::user_config::server_config::ServerConfig;

    /// 造一个订阅节点（sub1 归属，固定 uuid = 指纹 cred）。
    fn srv(name: &str, addr: &str, port: u16) -> ServerConfig {
        srv_uuid(name, addr, port, "11111111-1111-1111-1111-111111111111")
    }

    /// 造一个订阅节点（自定 uuid = 指纹 cred；同 uuid 同 host:port → 同指纹碰撞）。
    fn srv_uuid(name: &str, addr: &str, port: u16, uuid: &str) -> ServerConfig {
        serde_json::from_value(json!({
            "id": format!("gen-{name}"),
            "name": name,
            "protocol": "vless",
            "address": addr,
            "port": port,
            "subscriptionId": "sub1",
            "uuid": uuid,
        }))
        .expect("ServerConfig 应可反序列化")
    }

    #[test]
    fn reconcile_adds_updates_deletes_and_preserves_id() {
        let mut cfg = json!({
            "selectedServerId": "id-A",
            "servers": [
                // A 同指纹（同 uuid），但 name 不同（"OLD-A" vs 新 "A"）→ 内容变 → updated，保留 id-A。
                {"id":"id-A","name":"OLD-A","protocol":"vless","address":"a.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"},
                {"id":"id-B","name":"B","protocol":"vless","address":"b.com","port":443,"subscriptionId":"sub1","uuid":"u-b"},
                {"id":"id-X","name":"X","protocol":"vless","address":"x.com","port":443,"subscriptionId":"other"}
            ]
        });
        // A 指纹命中（name 变 → updated，保留 id-A）；C 新增；B 消失 → 删除；X（他订阅）不动。
        let new = vec![srv("A", "a.com", 443), srv("C", "c.com", 443)];
        let out = reconcile_subscription_servers(&mut cfg, "sub1", new, false, &[]);
        assert_eq!(out.added, 1, "C 新增");
        assert_eq!(out.updated, 1, "A 内容变（name）→ updated");
        assert_eq!(out.deleted, 1, "B 消失 → 删除");
        let servers = cfg["servers"].as_array().unwrap();
        assert!(
            servers.iter().any(|s| s["id"] == "id-X"),
            "他订阅节点不得被动"
        );
        let a = servers.iter().find(|s| s["name"] == "A").unwrap();
        assert_eq!(a["id"], "id-A", "命中须保留稳定 id（选中不失效）");
        assert!(!servers.iter().any(|s| s["name"] == "B"), "B 应删除");
        assert_eq!(cfg["selectedServerId"], "id-A", "选中仍在 → 不动");
    }

    #[test]
    fn reconcile_reselects_viable_fallback_when_selected_removed() {
        // 选中 id-B 被删；幸存的 C 是可承载全隧道的 vless → reselect 到 C（绝不裸 null / direct）。
        let mut cfg = json!({
            "selectedServerId": "id-B",
            "servers": [
                {"id":"id-B","name":"B","protocol":"vless","address":"b.com","port":443,"subscriptionId":"sub1","uuid":"u-b"}
            ]
        });
        let out = reconcile_subscription_servers(
            &mut cfg,
            "sub1",
            vec![srv("C", "c.com", 443)],
            false,
            &[],
        );
        assert_eq!(out.deleted, 1);
        assert_eq!(out.added, 1);
        // 幸存 vless 可作兜底 → 选它逃死节点（非 null、非 direct）。
        assert_eq!(
            cfg["selectedServerId"],
            json!("gen-C"),
            "悬挂选中 → 幸存可用节点"
        );
    }

    #[test]
    fn reconcile_no_change_when_identical() {
        let mut cfg = json!({
            "servers": [
                {"id":"id-A","name":"A","protocol":"vless","address":"a.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"}
            ]
        });
        let out = reconcile_subscription_servers(
            &mut cfg,
            "sub1",
            vec![srv("A", "a.com", 443)],
            false,
            &[],
        );
        assert_eq!(
            (out.added, out.updated, out.deleted),
            (0, 0, 0),
            "内容一致 → unchanged（不误报 updated）"
        );
    }

    #[test]
    fn reconcile_fingerprint_collision_keeps_both_with_distinct_ids() {
        // 两个**同指纹**（同 uuid 同 host:port，仅 name 异）现有订阅节点——真碰撞：FIFO 1:1 消费不丢 id。
        let mut cfg = json!({
            "servers": [
                {"id":"id-1","name":"HK-1","protocol":"vless","address":"cdn.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"},
                {"id":"id-2","name":"HK-2","protocol":"vless","address":"cdn.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"}
            ]
        });
        // 新集：两个同指纹节点（同 uuid 同 host:port）。
        let new = vec![
            srv_uuid(
                "HK-a",
                "cdn.com",
                443,
                "11111111-1111-1111-1111-111111111111",
            ),
            srv_uuid(
                "HK-b",
                "cdn.com",
                443,
                "11111111-1111-1111-1111-111111111111",
            ),
        ];
        let out = reconcile_subscription_servers(&mut cfg, "sub1", new, false, &[]);
        let servers = cfg["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2, "两个同指纹节点都保留（不因碰撞丢一个）");
        let ids: std::collections::HashSet<&str> =
            servers.iter().filter_map(|s| s["id"].as_str()).collect();
        assert_eq!(ids.len(), 2, "两个 id 各异（无重复 id）");
        assert!(
            ids.contains("id-1") && ids.contains("id-2"),
            "1:1 消费旧 id，不复用同一个"
        );
        assert_eq!((out.added, out.deleted), (0, 0), "两个都 1:1 命中，无增删");
    }

    #[test]
    fn reconcile_collision_shrink_deletes_extra_and_reselects_dangling() {
        // 选中 id-2；新集只剩一个同指纹节点 → 一个旧节点被删（FIFO 先消费 id-1，id-2 剩余 → 删）。
        // 悬挂 id-2 按实际结果 id 集判 → reselect 到幸存 id-1（可用 vless）。
        let mut cfg = json!({
            "selectedServerId": "id-2",
            "servers": [
                {"id":"id-1","name":"HK-1","protocol":"vless","address":"cdn.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"},
                {"id":"id-2","name":"HK-2","protocol":"vless","address":"cdn.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"}
            ]
        });
        let new = vec![srv_uuid(
            "HK-x",
            "cdn.com",
            443,
            "11111111-1111-1111-1111-111111111111",
        )];
        let out = reconcile_subscription_servers(&mut cfg, "sub1", new, false, &[]);
        assert_eq!(out.deleted, 1, "一个同指纹节点被删");
        let servers = cfg["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert!(
            !servers.iter().any(|s| s["id"] == "id-2"),
            "队尾未配对的 id-2 被删"
        );
        assert_eq!(
            cfg["selectedServerId"],
            json!("id-1"),
            "悬挂选中 → 幸存可用节点（非 null）"
        );
    }

    #[test]
    fn reconcile_keeps_direct_sentinel_selection() {
        // 直连哨兵 __direct__ 不是节点 id，实际结果 id 集里恒不存在，但绝不能被误改。
        let mut cfg = json!({
            "selectedServerId": "__direct__",
            "servers": [
                {"id":"id-A","name":"A","protocol":"vless","address":"a.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"}
            ]
        });
        // 空集：删光本订阅节点（reconcile 是纯函数，生产侧另有 0 节点 merge-only 前置守卫）。
        reconcile_subscription_servers(&mut cfg, "sub1", vec![], false, &[]);
        assert_eq!(
            cfg["selectedServerId"],
            json!("__direct__"),
            "直连哨兵不得被改"
        );
    }

    #[test]
    fn reconcile_merge_only_keeps_all_when_failed_provider_names_unknown() {
        // partial 但**失败 provider 名未知**（`failed_providers` 空）→ 退回整订阅级 merge-only：
        // 不删任何本订阅节点（防穿仓）。打断该兜底（→ 走删除分支）→ B 被删、deleted=1 → 断言转红。
        let mut cfg = json!({
            "selectedServerId": "id-B",
            "servers": [
                {"id":"id-A","name":"OLD-A","protocol":"vless","address":"a.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"},
                {"id":"id-B","name":"B","protocol":"vless","address":"b.com","port":443,"subscriptionId":"sub1","uuid":"u-b"}
            ]
        });
        // 新集只含 A（name 变 → updated）；B 缺席，但 merge_only → 保留 B、deleted=0。
        let out = reconcile_subscription_servers(
            &mut cfg,
            "sub1",
            vec![srv("A", "a.com", 443)],
            true,
            &[],
        );
        assert_eq!(out.deleted, 0, "merge_only → 不删任何存量（含缺席的 B）");
        assert_eq!(out.updated, 1, "A 仍正常更新");
        let servers = cfg["servers"].as_array().unwrap();
        assert!(
            servers.iter().any(|s| s["id"] == "id-B"),
            "B 保留（provider 临时失败不误删）"
        );
        assert_eq!(
            cfg["selectedServerId"],
            json!("id-B"),
            "选中 B 存活 → 不悬挂"
        );
    }

    /// M1 · **provider 级精确 merge-back**（本批修复的核心，上游 `leftoverToKeep`）。
    ///
    /// 场景：订阅有两个 provider，`P_fail` 拉取 503（transient）、`P_ok` 成功。两者名下各有一个节点
    /// 在本次不再出现（= 「下架」）。正确处置**必须区分**：
    /// - `P_fail` 名下的下架**不可信**（503 时我们根本没看到它的清单）→ 保留；
    /// - `P_ok` 名下的下架是**真下架** → 删除，且计入 `deleted`。
    ///
    /// 变异验证：把 `leftover_survives_partial` 的规则 3 去掉（改成恒 true = 旧的整订阅 merge-only）
    /// → `id-ok` 滞留、`deleted` 变 0 → 两条断言同时转红。
    #[test]
    fn reconcile_partial_deletes_only_from_succeeded_providers() {
        let mut cfg = json!({
            "servers": [
                // 失败 provider 名下的下架节点 —— 必须保留。
                {"id":"id-fail","name":"F","protocol":"vless","address":"f.com","port":443,
                 "subscriptionId":"sub1","uuid":"u-f","providerName":"P_fail"},
                // 成功 provider 名下的下架节点 —— 必须删除。
                {"id":"id-ok","name":"O","protocol":"vless","address":"o.com","port":443,
                 "subscriptionId":"sub1","uuid":"u-o","providerName":"P_ok"},
                // 主正文内联节点（无 providerName）—— 无归属信息 → 保守保留。
                {"id":"id-inline","name":"I","protocol":"vless","address":"i.com","port":443,
                 "subscriptionId":"sub1","uuid":"u-i"}
            ]
        });
        // 本次拉到的新集只含一个无关新节点 → 上面三个全落 leftover。
        let out = reconcile_subscription_servers(
            &mut cfg,
            "sub1",
            vec![srv("NEW", "n.com", 443)],
            true,
            &["P_fail".to_string()],
        );

        let servers = cfg["servers"].as_array().unwrap();
        let ids: std::collections::HashSet<&str> =
            servers.iter().filter_map(|s| s["id"].as_str()).collect();
        assert!(
            ids.contains("id-fail"),
            "失败 provider 名下的下架节点必须保留（503 时清单不可信）"
        );
        assert!(
            ids.contains("id-inline"),
            "无 providerName 的存量（内联/迁移前）无归属信息 → 保守保留"
        );
        assert!(
            !ids.contains("id-ok"),
            "成功 provider 名下的真下架节点必须删除（此前整订阅 merge-only 会让它无限滞留）"
        );
        assert_eq!(
            out.deleted, 1,
            "deleted 只计**实际**删掉的（此前 partial 恒 0 = 谎报无变化）"
        );
        assert_eq!(out.added, 1, "新节点正常入库");
    }

    /// M1 守卫：partial 下**选中节点**若落在成功 provider 名下且被真删 → 仍走 F14 reselect 兜底
    /// （不得留悬空引用）。此前 partial 恒保留一切，选中永不悬挂，故这条路径此前不可达。
    #[test]
    fn reconcile_partial_reselects_when_selected_deleted_by_succeeded_provider() {
        let mut cfg = json!({
            "selectedServerId": "id-ok",
            "servers": [
                {"id":"id-ok","name":"O","protocol":"vless","address":"o.com","port":443,
                 "subscriptionId":"sub1","uuid":"u-o","providerName":"P_ok"}
            ]
        });
        reconcile_subscription_servers(
            &mut cfg,
            "sub1",
            vec![srv("NEW", "n.com", 443)],
            true,
            &["P_fail".to_string()],
        );
        assert_eq!(
            cfg["selectedServerId"],
            json!("gen-NEW"),
            "选中被真删 → reselect 到幸存可用节点（绝不留悬空 id）"
        );
    }

    /// `leftover_survives_partial` 真值表（三条规则各一格 + 变异面）。
    #[test]
    fn leftover_keep_rules_truth_table() {
        let with_provider = json!({ "providerName": "P_fail" });
        let other_provider = json!({ "providerName": "P_ok" });
        let no_provider = json!({ "name": "inline" });
        let empty_provider = json!({ "providerName": "" });

        // 规则 1：失败名未知 → 全保留（退回整订阅 merge-only）。
        assert!(leftover_survives_partial(&with_provider, &[]));
        assert!(leftover_survives_partial(&other_provider, &[]));
        assert!(leftover_survives_partial(&no_provider, &[]));

        let failed = ["P_fail".to_string()];
        // 规则 2：无归属（缺键 / 空串）→ 保守保留。
        assert!(leftover_survives_partial(&no_provider, &failed));
        assert!(leftover_survives_partial(&empty_provider, &failed));
        // 规则 3：只保留失败 provider 名下的。
        assert!(leftover_survives_partial(&with_provider, &failed));
        assert!(
            !leftover_survives_partial(&other_provider, &failed),
            "成功 provider 名下的下架是真下架，不得保留"
        );
    }

    /// C-UA · 全局 `subscriptionUserAgent` 三级优先级（此前后端只读 per-sub，全局键是死键）。
    ///
    /// 变异验证：把 `resolve_subscription_ua` 的 `.or_else(全局)` 去掉 → 第二条断言转红；
    /// 把 per-sub 与全局取值顺序对调 → 第一条转红。
    #[test]
    fn subscription_ua_precedence_per_sub_then_global_then_default() {
        let cfg = json!({ "subscriptionUserAgent": "clash-verge/1.0" });

        // ① per-sub 优先于全局。
        assert_eq!(
            resolve_subscription_ua(&cfg, &json!({ "userAgent": "mihomo/1.18" })).as_deref(),
            Some("mihomo/1.18")
        );
        // ② per-sub 缺省 → 落全局（此前这里恒 None → 全局设置无效 → 机场按 UA 下发错格式）。
        assert_eq!(
            resolve_subscription_ua(&cfg, &json!({})).as_deref(),
            Some("clash-verge/1.0")
        );
        // ③ 两级皆缺 → None（交拉取层落 default_subscription_user_agent）。
        assert_eq!(resolve_subscription_ua(&json!({}), &json!({})), None);
        // ④ 空串/纯空白视同未设（不得把 `User-Agent: ` 空值发出去）。
        assert_eq!(
            resolve_subscription_ua(&cfg, &json!({ "userAgent": "   " })).as_deref(),
            Some("clash-verge/1.0"),
            "per-sub 空白应回落全局，而非发空 UA"
        );
        assert_eq!(
            resolve_subscription_ua(&json!({ "subscriptionUserAgent": "" }), &json!({})),
            None
        );
        // ⑤ 默认 UA 形态（拉取层兜底）——与契约注释 `Polaris/<版本>` 一致。
        assert_eq!(
            default_subscription_user_agent("9.9.9"),
            "Polaris/9.9.9",
            "默认 UA 须中性 Polaris/<ver>"
        );
    }

    #[test]
    fn reconcile_skips_subnet_only_mesh_falls_to_direct() {
        // #291：选中节点被删后，唯一幸存是「subnet-only 组网节点」（WG allowInternet:false，仅承载网段）
        // → 不可作兜底出口（否则公网静默走 direct = 泄漏）→ pick_viable 跳过 → 落 direct 哨兵。
        let mut cfg = json!({
            "selectedServerId": "id-sel",
            "servers": [
                {"id":"id-sel","name":"SEL","protocol":"vless","address":"s.com","port":443,"subscriptionId":"sub1","uuid":"u-sel"},
                {"id":"wg1","name":"WG","protocol":"wireguard","address":"w.com","port":51820,"subscriptionId":"sub1",
                 "wireguardSettings":{"allowInternet":false,"allowedIPs":["10.0.0.0/24"],"peerPublicKey":"pk"}}
            ]
        });
        // 新集只含 WG（同指纹 cred=peerPublicKey）；选中 vless 被删 → 幸存仅 subnet-only WG → 不可兜底 → direct。
        let wg: ServerConfig = serde_json::from_value(json!({
            "id":"gen-wg","name":"WG","protocol":"wireguard","address":"w.com","port":51820,"subscriptionId":"sub1",
            "wireguardSettings":{"allowInternet":false,"allowedIPs":["10.0.0.0/24"],"peerPublicKey":"pk"}
        })).unwrap();
        reconcile_subscription_servers(&mut cfg, "sub1", vec![wg], false, &[]);
        assert_eq!(
            cfg["selectedServerId"],
            json!("__direct__"),
            "subnet-only 组网节点不可作兜底 → 显式 direct（非静默泄漏）"
        );
    }

    #[test]
    fn fingerprint_matches_net_stack_typed() {
        // 跨类型等价锁：node_fingerprint(Value) ≡ net-stack server_fingerprint(&ServerConfig)。
        // 打断任一侧公式（改分隔符/字段/大小写）→ 断言转红。
        let sc = srv_uuid("HK", "cdn.com", 443, "u-xyz");
        let v = serde_json::to_value(&sc).unwrap();
        assert_eq!(
            node_fingerprint(&v),
            polaris_net_stack::subscription::server_fingerprint(&sc),
            "json 侧与 typed 侧指纹须一致"
        );
        assert_eq!(
            node_fingerprint(&v),
            "vless|cdn.com|443|u-xyz|tcp",
            "指纹形态"
        );
    }

    // ── A7 · subscription_delete 腿：选中随订阅删除 → →direct 哨兵 → 出口变（失效牙）─────
    // 打断 `apply_subscription_delete` 的置哨兵（不改选中）→ 断言转红；打断 `selected_exit_changed` → 转红。
    #[test]
    fn subscription_delete_sets_direct_sentinel_and_signals_exit_change() {
        let mut cfg = json!({
            "selectedServerId": "n1",
            "subscriptions": [{ "id": "sub1" }, { "id": "sub2" }],
            "servers": [
                { "id": "n1", "subscriptionId": "sub1" },
                { "id": "n2", "subscriptionId": "sub2" }
            ]
        });
        let old = cfg["selectedServerId"].as_str().map(str::to_string);
        apply_subscription_delete(&mut cfg, "sub1").expect("订阅存在 → Ok");
        let new = cfg.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(
            cfg["selectedServerId"],
            json!("__direct__"),
            "选中节点随订阅删除 → 置 direct 哨兵（绝不裸 null）"
        );
        assert!(
            !cfg["servers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == "n1"),
            "sub1 下节点删净"
        );
        assert!(
            selected_exit_changed(old.as_deref(), new),
            "→direct 是出口变（必须失效）"
        );
    }

    /// A7 · subscription_delete 守卫：选中属他订阅（删后仍存活）→ 选中不动 → 出口不变（不失效）。
    #[test]
    fn subscription_delete_keeps_unrelated_selection() {
        let mut cfg = json!({
            "selectedServerId": "n2",
            "subscriptions": [{ "id": "sub1" }, { "id": "sub2" }],
            "servers": [
                { "id": "n1", "subscriptionId": "sub1" },
                { "id": "n2", "subscriptionId": "sub2" }
            ]
        });
        let old = cfg["selectedServerId"].as_str().map(str::to_string);
        apply_subscription_delete(&mut cfg, "sub1").expect("订阅存在 → Ok");
        let new = cfg.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(new, Some("n2"), "选中属他订阅 → 存活不动");
        assert!(
            !selected_exit_changed(old.as_deref(), new),
            "选中未变 → 出口不变（不失效）"
        );
    }

    /// A7 · subscription_delete：`subscriptions` 在但无此 id → Err（命令层报「订阅不存在」）。
    #[test]
    fn subscription_delete_errs_on_missing_id() {
        let mut cfg = json!({ "subscriptions": [{ "id": "sub1" }], "servers": [] });
        assert!(
            apply_subscription_delete(&mut cfg, "nope").is_err(),
            "id 不存在 → Err"
        );
    }

    // ── A7 · subscription_update_servers（订阅刷新）腿：对账删选中 → reselect 兜底 → 出口变 ──
    // 打断 reconcile 的兜底 reselect → 断言转红；打断 `selected_exit_changed` → 转红。
    #[test]
    fn subscription_refresh_reselects_selected_signals_exit_change() {
        // 选中 id-B 被对账删除（新集只有 C）→ reselect 到幸存可用 C（→出口变）。
        let mut cfg = json!({
            "selectedServerId": "id-B",
            "servers": [
                {"id":"id-B","name":"B","protocol":"vless","address":"b.com","port":443,"subscriptionId":"sub1","uuid":"u-b"}
            ]
        });
        let old = cfg["selectedServerId"].as_str().map(str::to_string);
        reconcile_subscription_servers(&mut cfg, "sub1", vec![srv("C", "c.com", 443)], false, &[]);
        let new = cfg.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(
            cfg["selectedServerId"],
            json!("gen-C"),
            "选中随刷新消失 → reselect 幸存节点"
        );
        assert!(
            selected_exit_changed(old.as_deref(), new),
            "选中变 是出口变（必须失效）"
        );

        // 命中保留选中 id（同 id）→ 出口不变（不失效）。
        let mut cfg2 = json!({
            "selectedServerId": "id-A",
            "servers": [
                {"id":"id-A","name":"A","protocol":"vless","address":"a.com","port":443,"subscriptionId":"sub1","uuid":"11111111-1111-1111-1111-111111111111"}
            ]
        });
        let old2 = cfg2["selectedServerId"].as_str().map(str::to_string);
        reconcile_subscription_servers(&mut cfg2, "sub1", vec![srv("A", "a.com", 443)], false, &[]);
        let new2 = cfg2.get("selectedServerId").and_then(Value::as_str);
        assert_eq!(new2, Some("id-A"), "命中保留稳定 id");
        assert!(
            !selected_exit_changed(old2.as_deref(), new2),
            "选中 id 未变 → 出口不变（不失效）"
        );
    }
}

/// 订阅预检的**生产组合面门**（§K7.1 血证：mock 绿 ≠ 生产路径跑通）。
///
/// 这里驱动的是**生产唯一路径** [`preview_core`]，注入的 `client` 是**真** [`HttpRuntime`]
/// （真 reqwest），不是 mock HttpClient。三扇门：
/// 1. 真 client 注入 → command 核心真调 fetch → 真 socket 收字节 → 真解析出节点数（正门）；
/// 2. **变异**：把真 client 换成「连不上服务器」的 client（去掉 resolve 钉定）→ 门必须转红；
/// 3. SSRF guard 真的在生产路径上跑（用公网 IP 的 mock lookup 放行，用内网 IP 的 mock lookup 拒绝）。
#[cfg(test)]
mod production_gate {
    use super::*;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;

    use polaris_net_stack::ssrf::DnsLookup;

    use crate::runtime::http::HttpRuntime;

    /// 起一个回环 HTTP server，服一次 `body`（url-list 订阅正文）后退出。
    fn spawn_sub_server(body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().expect("取端口");
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });
        addr
    }

    /// mock DnsLookup：把订阅 hostname 解析到指定 IP（放行/拒绝由 IP 是否公网决定）。
    ///
    /// **注**：生产用 `SystemDnsLookup`（真系统解析），其正确性由 `runtime::http` 的
    /// `system_dns_*` 单测独立守。此处 mock 只为「把 SSRF 判定对象钉在一个已知公网 IP 上」，
    /// 从而**同时**证明「guard 真在生产路径跑」——传输落点另由 client 的 resolve 钉到回环。
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

    /// 两条合法 vless（url-list）：解析器接受 → nodeCount=2。
    const SUB_BODY: &str =
        "vless://11111111-1111-1111-1111-111111111111@a.com:443?encryption=none&type=tcp#nodeA\n\
vless://11111111-1111-1111-1111-111111111111@b.com:443?encryption=none&type=ws#nodeB";

    #[tokio::test]
    async fn preview_gate_real_client_injected_and_called_returns_real_nodes() {
        // ── 门 ①：真 HttpRuntime（真 reqwest）→ preview_core（生产唯一路径）→ 真 socket → 真解析 ──
        let addr = spawn_sub_server(SUB_BODY);
        // 真 client，DNS 钉定：sub.example.com 的传输落到回环 server。
        let client =
            HttpRuntime::with_resolve_overrides(&[("sub.example.com", addr)]).expect("建真 client");
        // SSRF guard 判定对象 = sub.example.com，解析到公网 IP → 放行（guard 真的跑了）。
        let lookup = FixedLookup("93.184.216.34");

        let result = preview_core(
            std::sync::Arc::new(client),
            &lookup,
            "http://sub.example.com/sub",
            false,
            None,
        )
        .await;

        assert_eq!(
            result.get("ok").and_then(Value::as_bool),
            Some(true),
            "真 client 注入 + command 核心真调 → 应拿到真数据，实得: {result}"
        );
        assert_eq!(
            result.get("nodeCount").and_then(Value::as_u64),
            Some(2),
            "应解析出 2 个真节点（真 socket 收的真正文），实得: {result}"
        );
    }

    #[tokio::test]
    async fn preview_gate_mutation_broken_client_injection_turns_red() {
        // ── 门 ②（变异验证）：打断真 client 注入 → 门必须转红 ──
        // 「打断」= 真 client 但传输落点被钉到**回环死端口**（127.0.0.1:1，无 listener）→ 连接立即被拒。
        // 这模拟「注入了一个连不到目标的 client」：证明门①的绿是**真的依赖成功传输**，不是恒绿摆设。
        // 刻意用回环死端口（非真公网 IP）：确定性拒连、零宿主网络触碰、无 15s 超时悬挂。
        let server_addr = spawn_sub_server(SUB_BODY);
        let _ = server_addr; // server 起了但 client 不连它 —— 正是「注入断裂」
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let client = HttpRuntime::with_resolve_overrides(&[("sub.example.com", dead)])
            .expect("建真 client（钉到死端口）");
        let lookup = FixedLookup("93.184.216.34"); // guard 放行，但传输落点是死端口

        let result = preview_core(
            std::sync::Arc::new(client),
            &lookup,
            "http://sub.example.com/sub",
            false,
            None,
        )
        .await;

        assert_eq!(
            result.get("ok").and_then(Value::as_bool),
            Some(false),
            "断开真 client 的传输可达性后，门必须转红（ok:false），实得: {result}"
        );
    }

    #[tokio::test]
    async fn preview_gate_ssrf_guard_runs_on_production_path() {
        // ── 门 ③：SSRF guard 真在生产路径跑 —— hostname 解析到内网 IP → 生产路径拒绝 ──
        // 这守的是「guard 没被接线时会静默放行内网」这类缺口（H1 DNS rebinding 防线在生产路径上活着）。
        let addr = spawn_sub_server(SUB_BODY);
        let client =
            HttpRuntime::with_resolve_overrides(&[("sub.example.com", addr)]).expect("建真 client");
        let lookup = FixedLookup("169.254.169.254"); // 云元数据地址 = 内网 → guard 必拒

        let result = preview_core(
            std::sync::Arc::new(client),
            &lookup,
            "http://sub.example.com/sub",
            false,
            None,
        )
        .await;

        assert_eq!(
            result.get("ok").and_then(Value::as_bool),
            Some(false),
            "解析到内网 IP 必须被 SSRF guard 拒（生产路径），实得: {result}"
        );
        assert_eq!(
            result.get("errorKind").and_then(Value::as_str),
            Some("ssrf"),
            "内网命中应归类 ssrf，实得: {result}"
        );
    }
}

/// C14 · 订阅经代理策略求值（真值表 + 配置键提取）。
///
/// 变异验证：打断 `Some("proxy") => true` / `Some("direct") => false` / follow 的 per-sub 读取任一，
/// 对应断言即转红——证明全局三态策略确实作用于「是否经代理」决策（此前后端零消费）。
#[cfg(test)]
mod proxy_policy_tests {
    use super::{resolve_subscription_via_proxy, want_proxy_for_sub};
    use serde_json::json;

    #[test]
    fn resolve_truth_table() {
        // proxy：强制经代理，忽略 per-sub。
        assert!(resolve_subscription_via_proxy(Some("proxy"), Some(false)));
        assert!(resolve_subscription_via_proxy(Some("proxy"), None));
        // direct：强制直连，忽略 per-sub。
        assert!(!resolve_subscription_via_proxy(Some("direct"), Some(true)));
        assert!(!resolve_subscription_via_proxy(Some("direct"), None));
        // follow / 未知 / 缺省：按 per-sub。
        assert!(resolve_subscription_via_proxy(Some("follow"), Some(true)));
        assert!(!resolve_subscription_via_proxy(Some("follow"), Some(false)));
        assert!(!resolve_subscription_via_proxy(Some("follow"), None));
        assert!(resolve_subscription_via_proxy(None, Some(true)));
        assert!(!resolve_subscription_via_proxy(None, None));
        // 未知策略值回落 follow（sanitize 前的脏值也不误判强制）。
        assert!(resolve_subscription_via_proxy(Some("bogus"), Some(true)));
        assert!(!resolve_subscription_via_proxy(Some("bogus"), Some(false)));
    }

    #[test]
    fn want_proxy_reads_global_and_per_sub_keys() {
        // 全局 proxy 覆盖 per-sub false。
        let cfg = json!({ "subscriptionProxyPolicy": "proxy" });
        let sub = json!({ "updateViaProxy": false });
        assert!(want_proxy_for_sub(&cfg, &sub), "全局 proxy 覆盖 per-sub 关");
        // 全局 direct 覆盖 per-sub true。
        let cfg = json!({ "subscriptionProxyPolicy": "direct" });
        let sub = json!({ "updateViaProxy": true });
        assert!(
            !want_proxy_for_sub(&cfg, &sub),
            "全局 direct 覆盖 per-sub 开"
        );
        // 缺全局键 → follow → 读 per-sub。
        let cfg = json!({});
        assert!(want_proxy_for_sub(&cfg, &json!({ "updateViaProxy": true })));
        assert!(!want_proxy_for_sub(
            &cfg,
            &json!({ "updateViaProxy": false })
        ));
        assert!(!want_proxy_for_sub(&cfg, &json!({})), "两键皆缺 → 直连");
    }

    /// 「**显式强制**」与「偏好」必须能分开 —— 静默回退只对后者合法。
    ///
    /// 变异锁：把 `proxy_policy_is_forced` 改成 `want_proxy_for_sub`（即 `follow` + per-sub 开
    /// 也算强制）→ `follow_with_per_sub_preference_is_not_forced` 断言转红：那会让自举期
    /// （核未起）的订阅更新在 follow 档下也一律失败，砸掉 上游的自举友好性。
    #[test]
    fn only_explicit_global_proxy_policy_counts_as_forced() {
        use super::proxy_policy_is_forced;
        assert!(proxy_policy_is_forced(
            &json!({ "subscriptionProxyPolicy": "proxy" })
        ));
        // follow + per-sub 开 = **偏好**，不是强制（静默回退直连仍合法，对齐 上游）。
        assert!(!proxy_policy_is_forced(
            &json!({ "subscriptionProxyPolicy": "follow" })
        ));
        assert!(!proxy_policy_is_forced(&json!({})));
        assert!(!proxy_policy_is_forced(
            &json!({ "subscriptionProxyPolicy": "direct" })
        ));
        // 脏值（sanitize 前）不得被误判成强制。
        assert!(!proxy_policy_is_forced(
            &json!({ "subscriptionProxyPolicy": "PROXY" })
        ));
        assert!(!proxy_policy_is_forced(
            &json!({ "subscriptionProxyPolicy": true })
        ));
    }
}

#[cfg(test)]
mod ua_tests {
    use super::{invalidate_validators_on_global_ua_change, resolve_subscription_ua, ua_changed};
    use serde_json::json;

    /// 三级链 + **falsy 语义**（与 TS `??` 的差异已登记在函数文档与 `contracts/types.ts`）。
    #[test]
    fn ua_falls_back_through_three_levels_with_falsy_empty() {
        let global = json!({ "subscriptionUserAgent": "clash-verge/1.0" });
        // per-sub 优先。
        assert_eq!(
            resolve_subscription_ua(&global, &json!({ "userAgent": "sing-box/1.9" })).as_deref(),
            Some("sing-box/1.9")
        );
        // per-sub 缺 → 全局。
        assert_eq!(
            resolve_subscription_ua(&global, &json!({})).as_deref(),
            Some("clash-verge/1.0")
        );
        // **差异格**：per-sub 显式空串 → falsy 回落全局（TS `??` 会让 `""` 胜出并发空 UA）。
        assert_eq!(
            resolve_subscription_ua(&global, &json!({ "userAgent": "" })).as_deref(),
            Some("clash-verge/1.0"),
            "空串按未设处理（发 `User-Agent: ` 空值会让机场下发错格式/0 节点）"
        );
        assert_eq!(
            resolve_subscription_ua(&global, &json!({ "userAgent": "   " })).as_deref(),
            Some("clash-verge/1.0"),
            "纯空白同理"
        );
        // 全局也空 → None（交默认 UA）。
        assert_eq!(
            resolve_subscription_ua(&json!({ "subscriptionUserAgent": "  " }), &json!({})),
            None
        );
        assert_eq!(resolve_subscription_ua(&json!({}), &json!({})), None);
    }

    /// UA 变更 → 条件 GET 验证器作废（否则机场按 UA 下发变体时永远 304 拿不到新格式）。
    ///
    /// 变异锁：把 `subscription_update` 里那段 `if ua_changed(...) { set_or_remove(..., None) }`
    /// 删掉 → `changing_ua_drops_validators` 转红。
    #[test]
    fn ua_changed_uses_the_same_falsy_normalisation_as_resolution() {
        let with = |ua: serde_json::Value| json!({ "id": "s1", "userAgent": ua });
        assert!(ua_changed(&with(json!("a")), &with(json!("b"))));
        assert!(ua_changed(&json!({ "id": "s1" }), &with(json!("a"))));
        assert!(ua_changed(&with(json!("a")), &json!({ "id": "s1" })));
        // 同值不算变（编辑名字/URL 不该白扔验证器 → 白丢一次条件 GET 的省流）。
        assert!(!ua_changed(&with(json!("a")), &with(json!("a"))));
        // 归一口径与 `resolve_subscription_ua` 一致：缺省/空串/纯空白三者互相之间**不算变更**
        //（它们求值出的实际 UA 完全相同，清验证器纯属白扔）。
        assert!(!ua_changed(&json!({}), &with(json!(""))));
        assert!(!ua_changed(&with(json!("")), &with(json!("   "))));
        // 名字变、UA 没变 → 不动验证器。
        assert!(!ua_changed(
            &json!({ "name": "old", "userAgent": "a" }),
            &json!({ "name": "new", "userAgent": "a" })
        ));
    }

    // ── 全局 `subscriptionUserAgent` 变更 → 验证器作废（LOW-1）────────────────────

    /// 两条订阅：`s-global` 靠全局 UA、`s-own` 自带 per-sub 覆盖。改全局 UA 后**只有前者**该被作废。
    ///
    /// 牙：
    /// - 删掉 `invalidate_validators_on_global_ua_change` 里的清除腿 → 第 1、2 条断言转红
    ///   （= 改了全局 UA 仍带旧 ETag 请求 → 机场恒 304 → 新格式永远拿不到）；
    /// - 把射程放宽成「全局变了就全清」（去掉 `per_sub.is_some()` 早退）→ 第 3、4 条转红
    ///   （白扔一次条件 GET，把下次更新变成全量下载）。
    #[test]
    fn global_ua_change_invalidates_only_subs_without_per_sub_override() {
        let old = json!({ "subscriptionUserAgent": "clash-verge/1.0" });
        let mut new = json!({
            "subscriptionUserAgent": "sing-box/1.9",
            "subscriptions": [
                { "id": "s-global", "etag": "W/\"v1\"", "lastModified": "Mon, 01 Jan 2024 00:00:00 GMT" },
                { "id": "s-own", "userAgent": "mihomo/1.18", "etag": "W/\"v2\"", "lastModified": "x" },
            ]
        });
        assert_eq!(
            invalidate_validators_on_global_ua_change(&old, &mut new),
            1,
            "只该有 1 条被作废（另一条有 per-sub 覆盖，生效 UA 没变）"
        );
        let subs = new["subscriptions"].as_array().unwrap();
        assert!(
            subs[0].get("etag").is_none(),
            "①靠全局 UA 的订阅：etag 必须清"
        );
        assert!(
            subs[0].get("lastModified").is_none(),
            "②靠全局 UA 的订阅：lastModified 必须清"
        );
        assert_eq!(
            subs[1]["etag"], "W/\"v2\"",
            "③per-sub 覆盖的订阅：生效 UA 与全局无关 → 验证器不得白扔"
        );
        assert_eq!(subs[1]["lastModified"], "x", "④同上");
    }

    /// 归一口径必须与 [`resolve_subscription_ua`] 一致：缺省 / `""` / 纯空白三者互换**不算变更**；
    /// 全局键没动时更不该动任何东西（改别的设置每次都清验证器 = 每次更新都全量下载）。
    ///
    /// 牙：把早退判据从 `pick_ua(...) == pick_ua(...)` 换成裸 `old.get(K) == new.get(K)` → 第 2、3 条转红。
    #[test]
    fn global_ua_normalisation_and_no_op_paths() {
        let subs = || json!([{ "id": "s1", "etag": "W/\"v1\"", "lastModified": "L" }]);
        // ① 全局键完全没动（改的是别的键）→ 零作废。
        let mut new = json!({ "subscriptionUserAgent": "ua/1", "logLevel": "debug", "subscriptions": subs() });
        assert_eq!(
            invalidate_validators_on_global_ua_change(
                &json!({ "subscriptionUserAgent": "ua/1" }),
                &mut new
            ),
            0
        );
        assert_eq!(new["subscriptions"][0]["etag"], "W/\"v1\"");
        // ② 空串 → 纯空白：求值出的 UA 完全相同，不算变更。
        let mut new = json!({ "subscriptionUserAgent": "   ", "subscriptions": subs() });
        assert_eq!(
            invalidate_validators_on_global_ua_change(
                &json!({ "subscriptionUserAgent": "" }),
                &mut new
            ),
            0
        );
        assert_eq!(new["subscriptions"][0]["etag"], "W/\"v1\"");
        // ③ 缺键 → 空串：同理不算变更。
        let mut new = json!({ "subscriptionUserAgent": "", "subscriptions": subs() });
        assert_eq!(
            invalidate_validators_on_global_ua_change(&json!({}), &mut new),
            0
        );
        assert_eq!(new["subscriptions"][0]["etag"], "W/\"v1\"");
        // ④ 缺键 → 真值：这是**真变更**（此前走 net-stack 默认 UA，现在走用户设的）。
        let mut new = json!({ "subscriptionUserAgent": "ua/2", "subscriptions": subs() });
        assert_eq!(
            invalidate_validators_on_global_ua_change(&json!({}), &mut new),
            1
        );
        assert!(new["subscriptions"][0].get("etag").is_none());
        // ⑤ 真值 → 清空：同样是真变更（回落默认 UA，变体可能又换一套）。
        let mut new = json!({ "subscriptionUserAgent": "", "subscriptions": subs() });
        assert_eq!(
            invalidate_validators_on_global_ua_change(
                &json!({ "subscriptionUserAgent": "ua/2" }),
                &mut new
            ),
            1
        );
        assert!(new["subscriptions"][0].get("lastModified").is_none());
    }

    /// 计数只算「真有验证器被扔掉」的订阅；无 `subscriptions` 键 / 空数组不得 panic。
    #[test]
    fn count_reflects_actually_dropped_validators_and_tolerates_missing_array() {
        let old = json!({ "subscriptionUserAgent": "a" });
        // 从没拉取过（无验证器）→ 不计数（否则日志谎报条数）。
        let mut new = json!({ "subscriptionUserAgent": "b", "subscriptions": [{ "id": "s1" }] });
        assert_eq!(invalidate_validators_on_global_ua_change(&old, &mut new), 0);
        // subscriptions 缺失 / 非数组 / 空数组 → 0，不 panic。
        let mut new = json!({ "subscriptionUserAgent": "b" });
        assert_eq!(invalidate_validators_on_global_ua_change(&old, &mut new), 0);
        let mut new = json!({ "subscriptionUserAgent": "b", "subscriptions": {} });
        assert_eq!(invalidate_validators_on_global_ua_change(&old, &mut new), 0);
        let mut new = json!({ "subscriptionUserAgent": "b", "subscriptions": [] });
        assert_eq!(invalidate_validators_on_global_ua_change(&old, &mut new), 0);
    }
}

/// 编排层（tauri command / 拉取编排）**接线**变异锁 —— 与「方法体单测」分工不同。
///
/// 本仓未引 `tauri::test`，命令壳带 `State<AppRuntime>` 无法在单测里直调；而本轮 review 明确点出
/// 「测方法体 ≠ 测接线」：净零序短路、UA 三级链、`failed_providers` 传递三处的**纯函数**全都有单测，
/// 但把编排层那一行改回旧行为（恒 save / 只读 per-sub / 丢名单）时，那些单测**一条都不会红**。
///
/// 故这里按本仓既有的源码扫描门（同 `runtime/rule_resource_scheduler.rs` 的
/// `scheduler_actually_wires_the_catalog_refresh_leg`）钉住调用点本身。它薄，但它抓的正是
/// 「纯函数还在、调用点没了」这一类假绿。
#[cfg(test)]
mod wiring_gate {
    const SRC: &str = include_str!("subscription.rs");

    /// **只扫生产正文**（`mod wiring_gate` 之前的那一段）。
    ///
    /// 这不是洁癖，是本门第一版真踩到的**自指假绿**：签名串本身就写在本模块的源码里，
    /// `SRC.find(sig)` 会命中**本模块自己**（尤其当真签名带泛型、与所给字面量不逐字相同时）
    /// —— 于是把生产代码整段改坏，断言照样在自己的字符串常量里找到那句话、照样绿。
    /// 截断到测试模块之前，自指变成 `panic!("找不到 …")`，一眼可见。
    fn production_src() -> &'static str {
        let end = SRC
            .find("mod wiring_gate {")
            .expect("本模块自身必然在 SRC 里");
        &SRC[..end]
    }

    /// 取顶层 `fn` 的源码片段（签名 → 首个顶格 `\n}`）。`sig` 必须与**真签名逐字**一致
    /// （含泛型参数），否则直接 panic —— 匹配不上比匹配错更安全。
    fn fn_body(sig: &str) -> &'static str {
        let src = production_src();
        let start = src
            .find(sig)
            .unwrap_or_else(|| panic!("生产正文里找不到 {sig}（签名要含泛型参数，逐字对齐）"));
        let rest = &src[start..];
        let end = rest.find("\n}").map_or(rest.len(), |p| p + 2);
        &rest[..end]
    }

    /// 门自身的自检：`fn_body` 必须真的取到**生产**函数，而不是本模块里的同名字面量。
    #[test]
    fn the_gate_scans_production_code_not_itself() {
        let body = fn_body("async fn perform_subscription_update_inner(");
        assert!(
            body.contains("state.config().load_full()"),
            "取到的必须是生产函数体（含真实实现语句），实得: {}",
            &body[..body.len().min(200)]
        );
        assert!(
            !production_src().contains("mod wiring_gate"),
            "扫描面不得包含本测试模块"
        );
    }

    #[test]
    fn update_resolves_ua_through_the_three_level_chain() {
        let body = fn_body("async fn perform_subscription_update_inner(");
        assert!(
            body.contains("resolve_subscription_ua(&cfg, &sub)"),
            "变异锁：改回内联 `sub.get(\"userAgent\")` → 全局 subscriptionUserAgent 重新变成死键，\
             而 `ua_tests` 里那些纯函数断言照样全绿"
        );
    }

    #[test]
    fn update_forwards_failed_provider_names_into_reconcile() {
        // ① 编排产出里必须真的接住这份名单（丢掉 → 整订阅级 merge-only、deleted 恒 0）。
        let fetch = fn_body("async fn fetch_parse_resolve<L: DnsLookup>(");
        assert!(
            fetch.contains("failed_providers = pres.failed_providers"),
            "变异锁：只取 any_failed、丢掉失败 provider 名单 → 成功 provider 名下的真下架节点永久滞留"
        );
        // ② 且必须一路传到 reconcile（provider 级精确 merge-back 的唯一入口）。
        let body = fn_body("async fn perform_subscription_update_inner(");
        assert!(
            body.contains("&outcome.failed_providers"),
            "变异锁：reconcile 收不到名单 → `leftover_survives_partial` 退化成全保留"
        );
    }

    #[test]
    fn forced_proxy_policy_is_checked_before_fetching() {
        let body = fn_body("async fn perform_subscription_update_inner(");
        let guard = body
            .find("proxy_policy_is_forced(&cfg)")
            .expect("变异锁：删掉强制经代理闸门 → 显式 policy=proxy 时静默明文直连拉订阅");
        let fetch = body
            .find("fetch_parse_resolve(")
            .expect("拉取腿仍在（本断言的锚点）");
        assert!(
            guard < fetch,
            "闸门必须在拉取**之前**：拉完再报错，DNS/SNI 已经泄漏出去了"
        );
    }

    /// UA 变更 → **在写回 config 之前**清掉条件 GET 验证器。
    ///
    /// 变异锁：删掉 `subscription_update` 里那段 `if ua_changed(..) { set_or_remove(..) }`
    /// → 本用例转红。`ua_tests::ua_changed_…` 测的是纯谓词，删掉调用点它照样全绿 ——
    /// 这正是本门存在的理由。
    #[test]
    fn changing_ua_drops_the_conditional_get_validators() {
        let body = fn_body("pub fn subscription_update(");
        let at = body
            .find("ua_changed(&arr[idx], &subscription)")
            .expect("变异锁：UA 变更未作废验证器 → 机场按 UA 下发变体时永远 304，新格式拿不到");
        for key in ["\"etag\"", "\"lastModified\""] {
            let cleared = body[at..]
                .find(&format!("set_or_remove(&mut subscription, {key}, None)"))
                .is_some();
            assert!(cleared, "{key} 必须在 UA 变更分支里被清掉");
        }
        // 且必须发生在写回之前（写回后再改就白改了）。
        let write_back = body.find("arr[idx] = subscription;").expect("写回腿仍在");
        assert!(at < write_back, "作废必须排在写回 config 之前");
    }

    /// **终态必达**由外壳结构保证，不是靠逐条早退各补一句 emit。
    #[test]
    fn the_update_shell_cannot_return_without_a_terminal_frame() {
        let shell = fn_body("pub(crate) async fn perform_subscription_update(");
        assert!(
            shell.contains("terminal_progress_frame(&result)"),
            "变异锁：删掉终态帧 → 订阅信息栏永远挂在「更新中」，且失败无处可见"
        );
        // 外壳里**一条早退都不许有**：有了就说明存在一条绕过终态帧的路径。
        assert!(
            !shell.contains("return"),
            "变异锁：往外壳里塞早退 = 重新造出「某些结局不发终态帧」这个原始缺陷"
        );
        // 终态只有一个发射者：内层自己再发一份就会与外壳打架（顺序/内容都无从保证）。
        let inner = fn_body("async fn perform_subscription_update_inner(");
        assert!(
            !inner.contains("terminal_progress_frame"),
            "变异锁：内层自发终态帧 → 一次更新两个终态，前端最后收到哪个取决于代码顺序"
        );
    }

    /// 起手帧必须排在真正开跑之前（否则「点了没反应」这个原始症状原样保留）。
    #[test]
    fn fetching_frame_precedes_the_real_work() {
        let shell = fn_body("pub(crate) async fn perform_subscription_update(");
        let emit = shell
            .find("\"phase\": \"fetching\"")
            .expect("变异锁：删掉起手帧 → 前 30s 屏幕上什么都不会变");
        let call = shell
            .find("perform_subscription_update_inner(")
            .expect("内层调用仍在（本断言的锚点）");
        assert!(emit < call, "起手帧必须在内层开跑之前发出");
    }

    /// 落盘/对账阶段单独报，且必须排在拉取腿之后。
    #[test]
    fn applying_phase_is_reported_after_the_network_leg() {
        let inner = fn_body("async fn perform_subscription_update_inner(");
        let fetch = inner
            .find("fetch_parse_resolve(")
            .expect("拉取腿仍在（本断言的锚点）");
        let applying = inner
            .find("\"phase\": \"applying\"")
            .expect("变异锁：删掉 applying → 本地落盘卡住时用户以为还在等机场");
        assert!(applying > fetch, "applying 必须在拉取之后");
    }

    /// provider 计数真的接进了子拉取闭包（纯逻辑单测覆盖计数语义，本门覆盖「有没有接上」）。
    #[test]
    fn provider_progress_counter_is_wired_into_the_fetch_closure() {
        let f = fn_body("async fn fetch_parse_resolve<L: DnsLookup>(");
        assert!(
            f.contains("ProviderProgress::new("),
            "变异锁：不建计数器 → provider 型订阅在最长 8×15s 里只有一个静止的「拉取中」"
        );
        let b = fn_body("fn build_provider_fetch(");
        assert!(
            b.contains("p.on_fetch_start()"),
            "变异锁：计数器建了却不调 → 分母有了、分子恒 0"
        );
    }

    /// 预检腿必须静音：那时还没有订阅 id，帧发出去没有归属（会以空 id 串到别的栏上）。
    #[test]
    fn preview_leg_emits_no_progress_frames() {
        let preview = fn_body("pub(crate) async fn preview_core<L: DnsLookup>(");
        assert!(
            preview.contains("fetch_parse_resolve("),
            "预检仍复用同一拉取层（本断言的锚点）"
        );
        assert!(
            !preview.contains("sink"),
            "变异锁：给预检也接上 sink → 新增订阅对话框会往一个还不存在的订阅推进度"
        );
        let inner = fn_body("async fn perform_subscription_update_inner(");
        assert!(
            inner.contains("Some(sink),"),
            "变异锁：更新腿传 None → provider 计数整条腿静默失联"
        );
    }

    /// 每一帧都必须带 `subscriptionId`：没有它，多订阅时前端无从判断该点亮哪一条信息栏。
    ///
    /// 只能扫源码——补 id 发生在 `BroadcastUpdateProgress::emit` 里，而构造它需要 `AppHandle`
    /// （本仓未引 `tauri::test`）。帧内容本身的门在 `mod progress_tests`。
    #[test]
    fn every_frame_is_stamped_with_the_subscription_id() {
        let body = fn_body("impl UpdateProgressSink for BroadcastUpdateProgress {");
        assert!(
            body.contains("\"subscriptionId\""),
            "变异锁：不补 subscriptionId → 帧无归属，前端要么全栏一起亮要么全不亮"
        );
    }
}

/// 订阅更新进度帧的**纯逻辑**门（`EVENT_SUBSCRIPTION_UPDATE_PROGRESS` 的载荷形状）。
///
/// 放在 `mod wiring_gate` **之后**是刻意的：`production_src()` 截断到 `mod wiring_gate {` 为止，
/// 本模块里的 `"phase"` / `"failed"` 等字面量因此不在扫描面内，不会把上面那些源码门喂成假绿。
#[cfg(test)]
mod progress_tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录器落点：单测无 `AppHandle`，靠它把「发了哪几帧」变成可断言的数据。
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<Value>>);

    impl UpdateProgressSink for RecordingSink {
        fn emit(&self, frame: Value) {
            self.0.lock().expect("测试锁未中毒").push(frame);
        }
    }

    impl RecordingSink {
        fn frames(&self) -> Vec<Value> {
            self.0.lock().expect("测试锁未中毒").clone()
        }
    }

    #[test]
    fn failure_frame_carries_the_real_backend_message() {
        let frame = terminal_progress_frame(&update_failure("订阅缺少 URL"));
        assert_eq!(frame["phase"], "failed");
        // 变异锁：换成「订阅更新失败」这类笼统兜底 → 订阅栏上那个 tooltip 就再也说不出是哪一步坏了。
        assert_eq!(frame["error"], "订阅缺少 URL");
    }

    #[test]
    fn missing_success_key_is_treated_as_failure_not_success() {
        // 防御性：契约破了也不能把「更新中」永远挂在栏上，更不能报一个假的「已完成」。
        let frame = terminal_progress_frame(&json!({}));
        assert_eq!(frame["phase"], "failed");
    }

    #[test]
    fn unchanged_is_not_collapsed_into_done_with_zero_counts() {
        let unchanged = terminal_progress_frame(&update_ok(0, 0, 0, true, None));
        let done_zero = terminal_progress_frame(&update_ok(0, 0, 0, false, None));
        // 变异锁：删掉 `unchanged` 分支 → 304 与「真跑了但零变化」在栏上长得一模一样，
        // 而这两件事对用户的下一步动作不同（前者订阅本来就没更新，后者才是「更新过了」）。
        assert_eq!(unchanged["phase"], "unchanged");
        assert_eq!(done_zero["phase"], "done");
    }

    #[test]
    fn done_frame_carries_real_reconcile_counts() {
        let frame = terminal_progress_frame(&update_ok(3, 1, 2, false, None));
        assert_eq!(frame["phase"], "done");
        // 变异锁：三个计数写串位（added/updated/deleted）在只看 phase 的断言下抓不到。
        assert_eq!(frame["added"], 3);
        assert_eq!(frame["updated"], 1);
        assert_eq!(frame["deleted"], 2);
    }

    #[test]
    fn provider_progress_reports_completed_count_starting_at_zero() {
        let sink = Arc::new(RecordingSink::default());
        let erased: Arc<dyn UpdateProgressSink> = sink.clone();
        let p = ProviderProgress::new(erased, 3);
        p.on_fetch_start();
        p.on_fetch_start();
        p.on_fetch_start();

        let frames = sink.frames();
        assert_eq!(frames.len(), 3, "三次子拉取 = 三帧");
        // 变异锁：`fetch_add` 换成「先加后读」→ 首帧变成 1/3，最后一帧变成 3/3 ——
        // 后者意味着「全部完成」，但此刻第三个 provider 其实一个字节都还没拉。
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f["phase"], "providers");
            assert_eq!(f["done"], i as u64, "done = 已完成数（发起前取值）");
            assert_eq!(f["total"], 3);
        }
    }
}
