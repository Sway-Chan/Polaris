//! 宽容反序列化（sanitize-don't-throw，维度7 #7）。
//!
//! Polaris 锚点：`ConfigManager.ts#loadConfig`（反序列化宽容、坏字段跳过不崩）+
//! `validateConfig` 中对各字段的逐项 sanitize（subscriptions/servers/customRules 逐项过滤坏条目、
//! 纯开关字段非 boolean 删除而非 throw）。
//!
//! 纪律（维度7 #7 HIGH）：反序列化绝不因单坏字段崩溃，更绝不清空用户配置。
//! 坏字段策略：
//!   - 结构性必填字段类型错（servers 非数组、proxyMode 非法等）→ [`StoreError::Validation`]，
//!     交 loadConfig catch（已备份不覆盖），与 TS validateConfig 的 throw 同口径。
//!   - 数组内单条坏元素 → 剔除该条、保留其余（servers/subscriptions/customRules/CIDR）。
//!   - 纯开关字段非 boolean → 删除（读取侧 `!== false` 视为默认开，语义自洽）。
//!   - 可选字段类型错 → 删除（回落默认）。

#![forbid(unsafe_code)]

use serde_json::{Map, Value};

/// 解析结果：成功得到清洗后的 Value（可直接 into 强类型），或结构错误。
pub type SanitizeResult = Result<Value, crate::StoreError>;

/// 宽容解析 JSON 字符串 → 清洗后的 [`Value`]。
///
/// 坏 JSON（SyntaxError）→ Err（交上层回落默认配置 + 备份损坏文件，**不覆盖**）。
/// JSON 合法但字段脏 → 逐字段 sanitize，坏字段剔除/删除，返回清洗后的 Value。
///
/// 这一层只做「JSON 形状清洗」；语义校验（端口范围、协议必填等）在 [`crate::validate`] 做。
pub fn sanitize_config(content: &str) -> SanitizeResult {
    let mut value: Value = serde_json::from_str(content).map_err(crate::StoreError::from)?;
    if !value.is_object() {
        // 顶层非对象 → 结构错误（交 catch 回落默认，不覆盖）。
        return Err(crate::StoreError::validation(
            "config root must be an object",
        ));
    }
    // 就地清洗：坏字段剔除 / 退化，绝不 throw。
    sanitize_value_in_place(&mut value);
    Ok(value)
}

/// 就地清洗 Value 树（已知字段逐项 sanitize）。坏字段删除，不 throw。
///
/// 仅清洗 ConfigManager.validateConfig 中会「就地改写 / 剔除单条」的字段；
/// 会 throw 的字段（proxyMode/proxyModeType/logLevel/tunConfig）留到 validate 阶段。
pub(crate) fn sanitize_value_in_place_pub(value: &mut Value) {
    sanitize_value_in_place(value)
}

fn sanitize_value_in_place(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    // ── 数组型字段：非数组删除（留 validate 处理必填语义）──────────────
    // servers / subscriptions / customRules / appRules / bypassProcesses / customAppPresets / rule_resources
    // TS validateConfig：servers/subscriptions 非数组 → throw（交 catch）；
    // customRules/appRules 非数组 → 视为 []（不 throw）。这里统一：非数组删除，
    // validate 阶段对必填的 servers 重新判定（缺失 → throw 或回落）。
    ensure_array_or_remove(obj, "servers");
    ensure_array_or_remove(obj, "subscriptions");
    ensure_array_or_remove(obj, "customRules");
    ensure_array_or_remove(obj, "appRules");
    ensure_array_or_remove(obj, "customAppPresets");
    ensure_array_or_remove(obj, "ruleResources");
    ensure_array_or_remove(obj, "bypassProcesses");
    ensure_array_or_remove(obj, "fakeIpFilterList");

    // ── 纯开关字段：非 boolean 删除（读取侧 !== false 视为开）──────────
    // appRoutingEnabled / singboxDashboard / mainSessionViaProxy / hardwareAcceleration /
    // windowEffects / fakeIpFilter / blockQuic / allowLan / bypassLAN / enableIPv6 /
    // interruptConnectionsOnSwitch / tlsFragment
    for key in [
        "appRoutingEnabled",
        "singboxDashboard",
        "mainSessionViaProxy",
        "hardwareAcceleration",
        "windowEffects",
        "fakeIpFilter",
        "blockQuic",
        "allowLan",
        "bypassLAN",
        "enableIPv6",
        "interruptConnectionsOnSwitch",
        "tlsFragment",
        "autoStart",
        "silentStart",
        "autoConnect",
        "minimizeToTray",
        "autoCheckUpdate",
        "autoLightweightMode",
        "keepTrayMenuWarm",
        "rememberWindowSize",
        "restartOnNodeChange",
        "disableLogFile",
        "desktopNotifications",
        "autoUpdateSubscriptionOnStart",
        "ruleResourceAutoUpdate",
        "autoPrivacyMode",
    ] {
        bool_or_remove(obj, key);
    }

    // restartOnNodeChange：TS validateConfig 归一为严格 boolean（=== true）。
    // 非 true → false（确保 switchMode 的真值转换不致 ON）。这里 sanitize 已删非 bool；
    // 若为 true 保留；缺失时不回填（读取端默认 false）。
    if matches!(obj.get("restartOnNodeChange"), Some(Value::Bool(true))) {
        // 保留 true
    } else if obj.contains_key("restartOnNodeChange") {
        obj.insert("restartOnNodeChange".into(), Value::Bool(false));
    }

    // ── 端口字段：非正整数删除（留 validate 校验范围）────────────────
    for key in ["mixedPort", "controlPort", "httpPort", "socksPort"] {
        port_or_remove(obj, key);
    }

    // ── 字符串字段：非字符串删除 ──────────────────────────────────
    // language（空串也删——TS 注释：留空串会让渲染端迁移 effect !language 守卫永不收敛）
    string_or_remove(
        obj, "language", /*trim*/ true, /*reject_empty*/ true,
    );
    string_or_remove(obj, "singboxDashboardUrl", true, true);
    string_or_remove(obj, "clashApiSecret", false, false);
    string_or_remove(obj, "speedTestUrl", false, false);
    // subscriptionProxyPolicy：三态，非法值删除（回落 follow）
    policy_or_remove(obj, "subscriptionProxyPolicy");
    // coreUpdateChannel：二态，非法值删除（回落 stable = 只看正式版，与改动前行为一致）
    core_channel_or_remove(obj, "coreUpdateChannel");

    // ── 对象字段：非对象删除 ─────────────────────────────────────
    // tunConfig / dnsConfig / regionRouting / builtinGeoMeta
    for key in ["tunConfig", "dnsConfig", "regionRouting"] {
        object_or_remove(obj, key);
    }
    // builtinGeoMeta：非对象删除（TS：读取侧全容错、无爆炸半径）
    object_or_remove(obj, "builtinGeoMeta");

    // ── selectedServerId：哨兵 '__direct__' / '__block__' 或 string 或 null ──────
    // 非 string/null 删除（留 validate 校验存在性）
    match obj.get("selectedServerId") {
        Some(Value::Null) | None => {}
        Some(Value::String(_)) => {}
        _ => {
            obj.remove("selectedServerId");
        }
    }

    // ── 嵌套：逐节点 / 逐订阅 / 逐规则清洗 ─────────────────────────
    sanitize_servers(obj);
    sanitize_subscriptions(obj);
    sanitize_custom_rules(obj);
    sanitize_tun_config(obj);
    sanitize_dns_config(obj);
}

/// 非 array → 删除键。
fn ensure_array_or_remove(obj: &mut Map<String, Value>, key: &str) {
    if let Some(v) = obj.get(key) {
        if !v.is_array() {
            obj.remove(key);
        }
    }
}

/// 非 bool → 删除键（读取侧 !== false 视为开，删除=回落默认开，语义自洽）。
fn bool_or_remove(obj: &mut Map<String, Value>, key: &str) {
    if let Some(v) = obj.get(key) {
        if !v.is_boolean() {
            obj.remove(key);
        }
    }
}

/// 非正整数（u16 范围）→ 删除键。
fn port_or_remove(obj: &mut Map<String, Value>, key: &str) {
    if let Some(v) = obj.get(key) {
        let ok = match v {
            Value::Number(n) => n
                .as_u64()
                .map(|u| (1..=65535).contains(&(u as u32)))
                .unwrap_or(false),
            _ => false,
        };
        if !ok {
            obj.remove(key);
        }
    }
}

/// 非字符串 / （可选）空白 → 删除键。
fn string_or_remove(obj: &mut Map<String, Value>, key: &str, trim: bool, reject_empty: bool) {
    if let Some(v) = obj.get_mut(key) {
        match v {
            Value::String(s) => {
                if trim {
                    let t = s.trim().to_string();
                    *s = t;
                }
                if reject_empty && s.trim().is_empty() {
                    obj.remove(key);
                }
            }
            _ => {
                obj.remove(key);
            }
        }
    }
}

/// subscriptionProxyPolicy 三态校验：非 follow/proxy/direct → 删除（回落 follow）。
fn policy_or_remove(obj: &mut Map<String, Value>, key: &str) {
    if let Some(Value::String(s)) = obj.get(key) {
        if !matches!(s.as_str(), "follow" | "proxy" | "direct") {
            obj.remove(key);
        }
    } else if obj.contains_key(key) {
        obj.remove(key);
    }
}

/// `coreUpdateChannel` 只认 `stable` / `prerelease`，其余（含非字符串）删除。
///
/// 删除而非回落写值：缺省本就等价 `stable`（读侧 `unwrap_or(false)`），写一个显式值只会在
/// config 里留下一条用户从没设过的记录。
fn core_channel_or_remove(obj: &mut Map<String, Value>, key: &str) {
    if let Some(Value::String(s)) = obj.get(key) {
        if !matches!(s.as_str(), "stable" | "prerelease") {
            obj.remove(key);
        }
    } else if obj.contains_key(key) {
        obj.remove(key);
    }
}

/// 非对象 → 删除键。
fn object_or_remove(obj: &mut Map<String, Value>, key: &str) {
    if let Some(v) = obj.get(key) {
        if !v.is_object() {
            obj.remove(key);
        }
    }
}

/// 逐节点清洗（servers[]）：剔除结构非法节点 + sanitize endpoint CIDR。
///
/// Polaris 锚点 validateConfig servers 段：缺 id/name/未知协议/缺 address/port/协议必填缺失 → 剔除该节点；
/// endpoint（WG/Tailscale）allowedIPs/routes/localAddress 非法 CIDR 丢弃保留合法；
/// allowInternet 非 bool 删除；Tailscale 单节点硬限（保留第一个）。
fn sanitize_servers(obj: &mut Map<String, Value>) {
    let Some(Value::Array(servers)) = obj.get_mut("servers") else {
        return;
    };
    let mut kept: Vec<Value> = Vec::with_capacity(servers.len());
    let mut first_tailscale = false;
    for s in servers.drain(..) {
        let Some(so) = s.as_object() else {
            continue; // 非对象 → 丢弃
        };
        // id / name 必须是非空字符串
        if !str_nonempty(so, "id") || !str_nonempty(so, "name") {
            continue;
        }
        // protocol 必须是已知协议（小写匹配）
        let Some(proto) = so.get("protocol").and_then(|v| v.as_str()) else {
            continue;
        };
        let proto_lower = proto.to_ascii_lowercase();
        if !crate::validate::is_allowed_protocol(&proto_lower) {
            continue;
        }
        // **无地址协议** / custom 豁免 address/port；其余必须有合法 address + port∈1..=65535。
        //
        // 2026-08-11 拆分：此前这里叫 `account_based` 且只含 tailscale，同一个变量既当
        // 「豁免 address/port」又当「单例硬限」的判据。tor 需要前者、**不需要**后者
        // （它是内嵌客户端，可以有多个；实测给内核传 `server` 直接 `unknown field "server"`，
        // 故它天生没有 address/port）。两个语义合用一个名字，加第二个成员时必然误伤。
        let addressless = matches!(proto_lower.as_str(), "tailscale" | "tor");
        let is_custom = proto_lower == "custom";
        if !addressless && !is_custom {
            if !str_nonempty(so, "address") {
                continue;
            }
            let Some(port) = so.get("port").and_then(|v| v.as_u64()) else {
                continue;
            };
            if !(1..=65535).contains(&port) {
                continue;
            }
        }
        // 协议必填校验（单一真值 crate::validate::protocol_requirement_ok）
        if !crate::validate::protocol_requirement_ok(&proto_lower, &s) {
            continue;
        }
        // Tailscale 单节点硬限（**只对 tailscale**，不随 `addressless` 扩大）
        if proto_lower == "tailscale" {
            if first_tailscale {
                continue;
            }
            first_tailscale = true;
        }
        // 拷贝并 sanitize endpoint CIDR
        let mut server = s.clone();
        if let Some(Value::Object(map)) = server.get_mut("wireguardSettings") {
            sanitize_cidr_list(map, "localAddress");
            sanitize_cidr_list(map, "allowedIPs");
            bool_or_remove(map, "allowInternet");
        }
        if let Some(Value::Object(map)) = server.get_mut("tailscaleSettings") {
            sanitize_cidr_list(map, "routes");
            sanitize_cidr_list(map, "advertiseRoutes");
            bool_or_remove(map, "allowInternet");
        }
        kept.push(server);
    }
    *servers = kept;
}

/// 逐订阅清洗（subscriptions[]）：缺 id/name/url → 剔除。
fn sanitize_subscriptions(obj: &mut Map<String, Value>) {
    let Some(Value::Array(subs)) = obj.get_mut("subscriptions") else {
        return;
    };
    let mut kept: Vec<Value> = Vec::with_capacity(subs.len());
    for s in subs.drain(..) {
        let Some(so) = s.as_object() else {
            continue;
        };
        if str_nonempty(so, "id") && str_nonempty(so, "name") && str_nonempty(so, "url") {
            kept.push(s);
        }
    }
    *subs = kept;
}

/// 逐规则清洗（customRules[]）：结构非法丢弃，值非法仅告警保留。
fn sanitize_custom_rules(obj: &mut Map<String, Value>) {
    let Some(Value::Array(rules)) = obj.get_mut("customRules") else {
        return;
    };
    let mut kept: Vec<Value> = Vec::with_capacity(rules.len());
    for r in rules.drain(..) {
        // 旧版 DomainRule（无 `type` + `domains` 数组）→ **原样保留**，交 migrate 阶段
        // （`migrate::migrate_custom_rules_domain_rule`）转新 Rule。此前 sanitize 先于 migrate 执行、又要求
        // `type` 非空，会在迁移前把无 type 的旧规则整条剥光 → `customRules==[]` 静默全丢（1 旧规则配置过 load
        // 即丢失）。对齐 上游 ConfigManager.validateConfig（L392-395：customRules 逐项校验前先 migrateCustomRules）——
        // 本 crate 把 migrate 拆成独立 pass，故在 sanitize 侧放行旧 shape 待迁移，达成同一 end-state。
        // migrate 产出恒为干净新 Rule（合法 type/values/action/enabled），无需回过 sanitize 逐项清洗。
        if crate::migrate::is_legacy_domain_rule(&r) {
            kept.push(r);
            continue;
        }
        let Some(ro) = r.as_object() else {
            continue;
        };
        // id 必须是非空字符串
        if !str_nonempty(ro, "id") {
            continue;
        }
        // type 必须是已知规则类型
        let Some(t) = ro.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if !crate::validate::is_known_rule_type(t) {
            continue;
        }
        // values 必须是数组
        if !ro.get("values").is_some_and(|v| v.is_array()) {
            continue;
        }
        // action 必须是 proxy/direct/block
        let action_ok = ro
            .get("action")
            .and_then(|v| v.as_str())
            .is_some_and(|a| matches!(a, "proxy" | "direct" | "block"));
        if !action_ok {
            continue;
        }
        // enabled 必须是 boolean
        if !ro.get("enabled").is_some_and(|v| v.is_boolean()) {
            continue;
        }
        let mut rule = r.clone();
        // 过滤 values 非字符串元素
        if let Some(Value::Array(vals)) = rule.get_mut("values") {
            vals.retain(|v| v.is_string());
        }
        // sanitize conditions（多条件）
        sanitize_rule_conditions(&mut rule);
        // combineMode 非 and/or 删除
        if let Some(cm) = rule.get("combineMode") {
            let ok = cm.as_str().is_some_and(|m| matches!(m, "and" | "or"));
            if !ok {
                if let Some(map) = rule.as_object_mut() {
                    map.remove("combineMode");
                }
            }
        }
        kept.push(rule);
    }
    *rules = kept;
}

/// sanitize rule.conditions（多条件）：非数组删除；逐条件结构校验、过滤非字符串值。
fn sanitize_rule_conditions(rule: &mut Value) {
    let Some(map) = rule.as_object_mut() else {
        return;
    };
    let conds_present = map.contains_key("conditions");
    if !conds_present {
        return;
    }
    let Some(Value::Array(conds)) = map.get_mut("conditions") else {
        // 非数组 → 删除（退化为单条件 type/values）
        map.remove("conditions");
        return;
    };
    let mut cleaned: Vec<Value> = Vec::with_capacity(conds.len());
    for c in conds.drain(..) {
        let Some(co) = c.as_object() else {
            continue;
        };
        let Some(t) = co.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if !crate::validate::is_known_rule_type(t) {
            continue;
        }
        let Some(vals) = co.get("values") else {
            continue;
        };
        if !vals.is_array() {
            continue;
        }
        let mut cond = c.clone();
        if let Some(Value::Array(v)) = cond.get_mut("values") {
            v.retain(|x| x.is_string());
            if v.is_empty() {
                continue; // 空值条件丢弃
            }
        }
        cleaned.push(cond);
    }
    if cleaned.is_empty() {
        map.remove("conditions");
    } else {
        // 镜像同步：type/values = 首 conditions（TS validateConfig 强制重镜像 cleaned[0]）
        let first = cleaned[0].clone();
        if let (Some(Value::String(t)), Some(Value::Array(v))) =
            (first.get("type").cloned(), first.get("values").cloned())
        {
            map.insert("type".into(), Value::String(t));
            map.insert("values".into(), Value::Array(v));
        }
        map.insert("conditions".into(), Value::Array(cleaned));
    }
}

/// CIDR 列表 sanitize：非数组保留；逐项非法丢弃（含范围校验）；空 → 删字段。
fn sanitize_cidr_list(map: &mut Map<String, Value>, key: &str) {
    let Some(v) = map.get_mut(key) else {
        return;
    };
    let Value::Array(list) = v else {
        // 非数组 → 删除（TS：list 非数组时 sanitizeCidrs 原样返回，但 endpoint CIDR 统一删）
        map.remove(key);
        return;
    };
    let cleaned: Vec<Value> = list
        .drain(..)
        .filter_map(|c| match c.as_str() {
            Some(s) => {
                let t = s.trim();
                if crate::validate::is_valid_ip_cidr(t) {
                    Some(Value::String(t.to_string()))
                } else {
                    None
                }
            }
            None => None,
        })
        .collect();
    if cleaned.is_empty() {
        map.remove(key);
    } else {
        *v = Value::Array(cleaned);
    }
}

/// tunConfig.inboundExcludeCidrs sanitize（normalizeTunExcludeCidr：裸 IP 补 /32|/128、
/// 拒 catch-all/过宽/非法；空/全非法 → 删字段避免 [] 触发 norm 翻转）。
fn sanitize_tun_config(obj: &mut Map<String, Value>) {
    let Some(Value::Object(tun)) = obj.get_mut("tunConfig") else {
        return;
    };
    let present = tun.contains_key("inboundExcludeCidrs");
    if !present {
        return;
    }
    let Some(Value::Array(list)) = tun.get_mut("inboundExcludeCidrs") else {
        tun.remove("inboundExcludeCidrs"); // 非数组 → 删
        return;
    };
    let mut cleaned: Vec<String> = Vec::new();
    for c in list.drain(..) {
        if let Some(s) = c.as_str() {
            if let Some(n) = crate::validate::normalize_tun_exclude_cidr(s) {
                cleaned.push(n);
            }
        }
    }
    // 去重保序
    cleaned = crate::dedupe_str(&cleaned);
    if cleaned.is_empty() {
        tun.remove("inboundExcludeCidrs");
    } else {
        *list = cleaned.into_iter().map(Value::String).collect();
    }
}

/// dnsConfig.dnsTimeoutMs sanitize：须 1..=60000 有限正整数，否则删除；非整数取整。
fn sanitize_dns_config(obj: &mut Map<String, Value>) {
    let Some(Value::Object(dns)) = obj.get_mut("dnsConfig") else {
        return;
    };
    if let Some(v) = dns.get("dnsTimeoutMs") {
        let ok = v
            .as_f64()
            .map(|ms| ms.is_finite() && (1.0..=60000.0).contains(&ms))
            .unwrap_or(false);
        if ok {
            if let Some(n) = v.as_f64() {
                if !n.fract().eq(&0.0) {
                    dns.insert("dnsTimeoutMs".into(), Value::from(n.round() as i64));
                }
            }
        } else {
            dns.remove("dnsTimeoutMs");
        }
    }
}

/// 字段是否为非空字符串。
fn str_nonempty(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key)
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_json_returns_parse_err() {
        let res = sanitize_config("{ not valid json");
        assert!(matches!(res, Err(crate::StoreError::Parse(_))));
    }

    #[test]
    fn bad_field_skipped_good_field_kept() {
        // servers 是 string（坏字段）→ 删除；但 customRules 合法 → 保留。
        // 绝不因 servers 坏而丢弃整份配置（维度7 #7 核心）。
        let json = r#"{
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "servers": "not-an-array",
            "customRules": [
                {"id":"r1","type":"domain","values":["a.com"],"action":"proxy","enabled":true}
            ],
            "mixedPort": "bad-port"
        }"#;
        let v = sanitize_config(json).unwrap();
        let obj = v.as_object().unwrap();
        // 坏字段已删
        assert!(!obj.contains_key("servers"));
        assert!(!obj.contains_key("mixedPort"));
        // 好字段保留
        assert_eq!(obj["proxyMode"], "global");
        assert_eq!(obj["customRules"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn bad_server_dropped_good_server_kept() {
        let json = r#"{
            "proxyMode": "global",
            "proxyModeType": "tun",
            "servers": [
                {"id":"good","name":"Good","protocol":"trojan","address":"1.2.3.4","port":443,"password":"pw"},
                {"id":"","name":"Bad","protocol":"trojan"},
                {"id":"unknown-proto","name":"X","protocol":"nonexistent","address":"5.6.7.8","port":80}
            ]
        }"#;
        let v = sanitize_config(json).unwrap();
        let servers = v["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["id"], "good");
    }

    #[test]
    fn bool_field_bad_removed() {
        let json = r#"{
            "proxyMode": "direct",
            "proxyModeType": "manual",
            "appRoutingEnabled": "yes",
            "singboxDashboard": 1,
            "keepTrayMenuWarm": "yes",
            "hardwareAcceleration": true
        }"#;
        let v = sanitize_config(json).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("appRoutingEnabled"));
        assert!(!obj.contains_key("singboxDashboard"));
        assert!(!obj.contains_key("keepTrayMenuWarm"));
        assert_eq!(obj["hardwareAcceleration"], Value::Bool(true));
    }

    #[test]
    fn legacy_domain_rule_preserved_garbage_dropped() {
        // 项1：无 `type` + `domains` 数组的旧 DomainRule 不被 sanitize 丢弃（原样留待 migrate 转 Rule）；
        // 无 type 且**无 domains** 的纯垃圾条目仍照丢。判据复用 migrate::is_legacy_domain_rule（单一真值）。
        let json = r#"{
            "proxyMode":"global","proxyModeType":"systemProxy",
            "customRules":[
                {"id":"legacy","domains":["a.com"],"action":"proxy","enabled":true},
                {"id":"garbage","action":"proxy","enabled":true}
            ]
        }"#;
        let v = sanitize_config(json).unwrap();
        let rules = v["customRules"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "旧 DomainRule 保留、纯垃圾丢弃");
        assert_eq!(rules[0]["id"], "legacy");
        assert!(
            rules[0].get("type").is_none(),
            "sanitize 不给旧规则补 type（交 migrate 处理）"
        );
    }

    #[test]
    fn root_non_object_is_error() {
        let res = sanitize_config("[1,2,3]");
        assert!(matches!(res, Err(crate::StoreError::Validation(_))));
    }

    #[test]
    fn empty_input_is_error() {
        let res = sanitize_config("");
        assert!(matches!(res, Err(crate::StoreError::Parse(_))));
    }
}
