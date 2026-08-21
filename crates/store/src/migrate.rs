//! 一次性迁移链（维度7 #54）。
//!
//! Polaris 锚点：`ConfigManager.ts` 的 migrate* 方法（loadConfig 内顺序调用）+
//! validateConfig 中的旧数据迁移（customRules DomainRule→Rule / bypassProcesses→Rule / mixedPort 收敛）。
//!
//! 迁移纪律（维度7 #54）：幂等（标记守卫）+ 绝不抛（吞异常）+ 落盘失败不阻断。
//! 纯逻辑：本模块仅做就地改写 Value，不触碰 FS；落盘由 store 层 best-effort 驱动。
//!
//! 迁移链顺序（与 TS loadConfig 一致）：
//!   1. validateConfig 内：mixedPort 收敛（删 httpPort/socksPort）/ customRules DomainRule→Rule /
//!      bypassProcesses→Rule（均在 sanitize 后的 Value 上幂等执行）
//!   2. migrateFakeIpToggle（缺 dnsConfig 补齐 / enableFakeIp 按模式冻结 + 标记）
//!   3. migrateFakeIpTunPending（systemProxy+false+migrated → fakeIpTunAutoEnable=true）
//!   4. migrateNodeResolver（nodeDomainResolver → nodeResolverPool/Single + 标记）
//!   5. migrateSubscriptionProxyPolicy（旧布尔 subscriptionUpdateViaProxy → 三态）
//!   6. migrateTunStack（存量 stack → 'auto' + 标记）
//!   7. migrateTunMtu（抹掉存量 tunConfig.mtu → 缺席即自动 + 标记）
//!   8. migrateTrayMenuWarmDefault（中间构建写入的旧默认 false → 最终默认 true + 标记）
//!   9. migrateDiagnosticCapture（撤掉的诊断采集机制：还原 logLevel + 清孤儿键）
//!  10. appRulesSeeded（默认注入内置预设 + 剔除下线预设）

#![forbid(unsafe_code)]

use serde_json::{Map, Value};

/// 迁移结果：标记本次是否有变更（true → 调用方应 best-effort 落盘）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationDelta {
    pub changed: bool,
}

/// 执行全量迁移链（在 sanitize + validate 之后的 Value 上）。
///
/// 幂等：所有迁移均有标记守卫，已迁移跳过。绝不抛：内部不返回 Err。
/// 返回 [`MigrationDelta`] 供调用方决定是否落盘。
pub fn migrate_all(value: &mut Value) -> MigrationDelta {
    let mut delta = MigrationDelta::default();
    // validateConfig 内联迁移（无独立标记，但本身幂等）
    delta.changed |= migrate_mixed_port(value);
    delta.changed |= migrate_custom_rules_domain_rule(value);
    delta.changed |= migrate_bypass_processes(value);
    // loadConfig 顺序迁移（带标记）
    migrate_fake_ip_toggle(value, &mut delta);
    migrate_fake_ip_tun_pending(value, &mut delta);
    migrate_node_resolver(value, &mut delta);
    migrate_subscription_proxy_policy(value, &mut delta);
    migrate_tun_stack(value, &mut delta);
    migrate_tun_mtu(value, &mut delta);
    migrate_tray_menu_warm_default(value, &mut delta);
    migrate_diagnostic_capture(value, &mut delta);
    delta.changed |= seed_app_rules(value);
    // F29 隐私密码：纯逻辑侧仅清内存明文（防 CONFIG_GET_VALUE 外泄）；
    // 哈希落盘（writePrivacyHash）属运行时层 FS 职责，经 ConfigFs 扩展方法注入。
    delta.changed |= migrate_privacy_password_clear(value);
    delta
}

/// 隐私密码明文清除（F29 纯逻辑侧）：config.privacyPassword 非空 → 内存清空（置 ""）。
///
/// Polaris 锚点 loadConfig F29 段：即便后续哈希落盘 fs 失败，明文也不会再经 CONFIG_GET_VALUE /
/// configChanged 外泄。哈希计算 + 落盘（writePrivacyHash）属运行时层职责（需 FS + crypto），
/// 此处仅做「先清内存明文」的纯逻辑改写，幂等（已 "" 不再变）。
pub fn migrate_privacy_password_clear(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let nonempty = obj
        .get("privacyPassword")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    if nonempty {
        obj.insert("privacyPassword".into(), Value::String(String::new()));
        true
    } else {
        false
    }
}

/// mixed-only 收敛：mixedPort 未设→旧 httpPort（忽略 65533 哨兵）→ 默认 7890；删 httpPort/socksPort。
/// Polaris validateConfig mixedPort 段。幂等：mixedPort>0 不改写。
pub fn migrate_mixed_port(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let mixed = obj.get("mixedPort").and_then(|v| v.as_u64());
    let need = mixed.is_none_or(|p| p == 0);
    let mut changed = false;
    if need {
        let legacy_http = obj
            .get("httpPort")
            .and_then(|v| v.as_u64())
            .filter(|&p| p > 0 && p != 65533);
        let port = legacy_http.unwrap_or(7890);
        obj.insert("mixedPort".into(), Value::from(port));
        changed = true;
    }
    if obj.remove("httpPort").is_some() {
        changed = true;
    }
    if obj.remove("socksPort").is_some() {
        changed = true;
    }
    changed
}

/// 旧 DomainRule（无 type、有 domains 数组）→ 新 Rule[]（domainSuffix / geosite / ipCidr）。
/// 上游 `migrateCustomRules` + `migrateLegacyDomainRule`。幂等：已是新 shape 原样保留。
pub fn migrate_custom_rules_domain_rule(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let Some(Value::Array(rules)) = obj.get_mut("customRules") else {
        return false;
    };
    let original = rules.clone();
    let mut out: Vec<Value> = Vec::new();
    for r in rules.drain(..) {
        if is_legacy_domain_rule(&r) {
            out.extend(migrate_legacy_domain_rule(&r));
        } else if r.as_object().is_some_and(|o| o.contains_key("type")) {
            out.push(r);
        }
        // 其余脏数据丢弃
    }
    let changed = out != original;
    *rules = out;
    changed
}

/// customRules 是否含任何旧版 DomainRule（决定「是否需迁移 + 迁移前备份」）。
/// 上游 `customRulesNeedMigration`（shared/rules.ts:355）。供 store::load 判「起迁移前落 .pre-rule-migration.bak」。
#[must_use]
pub fn custom_rules_need_migration(value: &Value) -> bool {
    value
        .get("customRules")
        .and_then(|v| v.as_array())
        .is_some_and(|rules| rules.iter().any(is_legacy_domain_rule))
}

/// 是否旧版 DomainRule（无 type 字段且 domains 为数组）。上游 `isLegacyDomainRule`。
///
/// `pub(crate)`：sanitize 侧需在丢弃无 `type` 条目**之前**放行旧 shape（待本模块 `migrate_custom_rules_domain_rule`
/// 转 Rule），复用此单一判据，避免与 sanitize 侧另写一份旧规则识别逻辑而漂移。
pub(crate) fn is_legacy_domain_rule(r: &Value) -> bool {
    let Some(o) = r.as_object() else {
        return false;
    };
    !o.contains_key("type") && o.get("domains").is_some_and(|d| d.is_array())
}

/// 旧 DomainRule → 新 Rule[]（无损、幂等）。上游 `migrateLegacyDomainRule`。
fn migrate_legacy_domain_rule(old: &Value) -> Vec<Value> {
    let Some(o) = old.as_object() else {
        return vec![];
    };
    let id = o
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let action = o
        .get("action")
        .and_then(|v| v.as_str())
        .filter(|a| matches!(*a, "proxy" | "direct" | "block"))
        .unwrap_or("proxy");
    let enabled = o.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let bypass_fakeip = o.get("bypassFakeIP").cloned();
    let target = o.get("targetServerId").cloned();
    let remarks = o.get("remarks").cloned();

    let mut domains: Vec<String> = Vec::new();
    let mut geosite_tags: Vec<String> = Vec::new();
    if let Some(Value::Array(arr)) = o.get("domains") {
        for d in arr {
            if let Some(s) = d.as_str() {
                let v = s.trim();
                if v.is_empty() {
                    continue;
                }
                if v.to_ascii_lowercase().starts_with("geosite:") {
                    let tag = v[8..].trim();
                    if !tag.is_empty() {
                        geosite_tags.push(tag.to_string());
                    }
                } else {
                    domains.push(v.strip_prefix("*.").unwrap_or(v).to_string());
                }
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();
    if !domains.is_empty() {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(id.clone()));
        m.insert("type".into(), Value::String("domainSuffix".into()));
        m.insert(
            "values".into(),
            Value::Array(domains.into_iter().map(Value::String).collect()),
        );
        m.insert("action".into(), Value::String(action.into()));
        m.insert("enabled".into(), Value::Bool(enabled));
        if let Some(b) = bypass_fakeip.clone() {
            m.insert("bypassFakeIP".into(), b);
        }
        if let Some(t) = target.clone() {
            m.insert("targetServerId".into(), t);
        }
        if let Some(r) = remarks.clone() {
            m.insert("remarks".into(), r);
        }
        out.push(Value::Object(m));
    }
    if !geosite_tags.is_empty() {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(format!("{id}_geosite")));
        m.insert("type".into(), Value::String("geosite".into()));
        m.insert(
            "values".into(),
            Value::Array(geosite_tags.into_iter().map(Value::String).collect()),
        );
        m.insert("action".into(), Value::String(action.into()));
        m.insert("enabled".into(), Value::Bool(enabled));
        if let Some(t) = target.clone() {
            m.insert("targetServerId".into(), t);
        }
        if let Some(r) = remarks.clone() {
            m.insert("remarks".into(), r);
        }
        out.push(Value::Object(m));
    }
    let mut ip_cidrs: Vec<String> = Vec::new();
    if let Some(Value::Array(arr)) = o.get("ipCidr") {
        for c in arr {
            if let Some(s) = c.as_str() {
                if !s.trim().is_empty() {
                    ip_cidrs.push(s.trim().to_string());
                }
            }
        }
    }
    if !ip_cidrs.is_empty() {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(format!("{id}_ip")));
        m.insert("type".into(), Value::String("ipCidr".into()));
        m.insert(
            "values".into(),
            Value::Array(ip_cidrs.into_iter().map(Value::String).collect()),
        );
        m.insert("action".into(), Value::String(action.into()));
        m.insert("enabled".into(), Value::Bool(enabled));
        if let Some(t) = target.clone() {
            m.insert("targetServerId".into(), t);
        }
        if let Some(r) = remarks.clone() {
            let base = r.as_str().unwrap_or("");
            m.insert("remarks".into(), Value::String(format!("{base} (IP)")));
        } else {
            m.insert("remarks".into(), Value::String(" (IP)".into()));
        }
        out.push(Value::Object(m));
    }
    // 极端兜底：既无域名也无 ipCidr → 空 domainSuffix 占位（避免规则凭空消失）
    if out.is_empty() {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(id));
        m.insert("type".into(), Value::String("domainSuffix".into()));
        m.insert("values".into(), Value::Array(vec![]));
        m.insert("action".into(), Value::String(action.into()));
        m.insert("enabled".into(), Value::Bool(enabled));
        if let Some(b) = bypass_fakeip {
            m.insert("bypassFakeIP".into(), b);
        }
        if let Some(t) = target {
            m.insert("targetServerId".into(), t);
        }
        if let Some(r) = remarks {
            m.insert("remarks".into(), r);
        }
        out.push(Value::Object(m));
    }
    out
}

/// 旧「排除进程」bypassProcesses → customRules processName+direct 规则。Polaris validateConfig bypassProcesses 段。
/// 固定 id 'migrated_bypass_processes' 天然幂等；迁移后清空 bypassProcesses。
pub fn migrate_bypass_processes(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let bypass = obj
        .get("bypassProcesses")
        .and_then(|v| v.as_array())
        .cloned();
    let nonempty = bypass.as_ref().is_some_and(|a| !a.is_empty());
    if !nonempty {
        // 确保字段存在为 []（TS validateConfig 兜底）
        if !obj.contains_key("bypassProcesses") {
            obj.insert("bypassProcesses".into(), Value::Array(vec![]));
            return true;
        }
        return false;
    }
    let mut values: Vec<String> = Vec::new();
    for p in bypass.unwrap() {
        if let Some(s) = p.as_str() {
            let t = s.trim();
            if !t.is_empty() {
                values.push(t.to_string());
            }
        }
    }
    values = crate::dedupe_str(&values);
    let already = obj
        .get("customRules")
        .and_then(|v| v.as_array())
        .is_some_and(|rules| {
            rules
                .iter()
                .any(|r| r.get("id").and_then(|v| v.as_str()) == Some("migrated_bypass_processes"))
        });
    if !values.is_empty() && !already {
        let mut m = Map::new();
        m.insert(
            "id".into(),
            Value::String("migrated_bypass_processes".into()),
        );
        m.insert("type".into(), Value::String("processName".into()));
        m.insert(
            "values".into(),
            Value::Array(values.into_iter().map(Value::String).collect()),
        );
        m.insert("action".into(), Value::String("direct".into()));
        m.insert("enabled".into(), Value::Bool(true));
        m.insert(
            "remarks".into(),
            Value::String("排除进程（自动迁移）".into()),
        );
        let rules = obj
            .entry("customRules")
            .or_insert_with(|| Value::Array(vec![]));
        if let Value::Array(arr) = rules {
            arr.push(Value::Object(m));
        }
    }
    obj.insert("bypassProcesses".into(), Value::Array(vec![]));
    true
}

/// FakeIP 开关统一一次性迁移。上游 `migrateFakeIpToggle`。
/// 缺 dnsConfig → 补默认；否则 fakeIpToggleMigrated!==true 时按模式冻结 enableFakeIp + 置标记。
pub fn migrate_fake_ip_toggle(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let mode_lower = obj
        .get("proxyModeType")
        .and_then(|v| v.as_str())
        .unwrap_or("systemProxy")
        .to_ascii_lowercase();
    if obj.get("dnsConfig").is_none_or(|v| !v.is_object()) {
        let enable = mode_lower != "systemproxy";
        let mut dns = Map::new();
        dns.insert(
            "domesticDns".into(),
            Value::String("https://doh.pub/dns-query".into()),
        );
        dns.insert(
            "foreignDns".into(),
            Value::String("https://dns.google/dns-query".into()),
        );
        dns.insert("enableFakeIp".into(), Value::Bool(enable));
        dns.insert("fakeIpToggleMigrated".into(), Value::Bool(true));
        obj.insert("dnsConfig".into(), Value::Object(dns));
        delta.changed = true;
        return;
    }
    let dns = obj
        .get_mut("dnsConfig")
        .and_then(|v| v.as_object_mut())
        .unwrap();
    if dns.get("fakeIpToggleMigrated") != Some(&Value::Bool(true)) {
        let enable = if mode_lower != "systemproxy" {
            true
        } else {
            dns.get("enableFakeIp")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        dns.insert("enableFakeIp".into(), Value::Bool(enable));
        dns.insert("fakeIpToggleMigrated".into(), Value::Bool(true));
        delta.changed = true;
    }
}

/// FakeIP-TUN 待纠正快照评估。上游 `migrateFakeIpTunPending`。
/// fakeIpTunAutoEnable===undefined 时评估：systemProxy + enableFakeIp===false + migrated → true，否则 false。
pub fn migrate_fake_ip_tun_pending(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let mode_lower = obj
        .get("proxyModeType")
        .and_then(|v| v.as_str())
        .unwrap_or("systemProxy")
        .to_ascii_lowercase();
    let Some(dns) = obj.get_mut("dnsConfig").and_then(|v| v.as_object_mut()) else {
        return;
    };
    if dns.contains_key("fakeIpTunAutoEnable") {
        return;
    }
    let pending = mode_lower == "systemproxy"
        && dns.get("enableFakeIp").and_then(|v| v.as_bool()) == Some(false)
        && dns.get("fakeIpToggleMigrated") == Some(&Value::Bool(true));
    dns.insert("fakeIpTunAutoEnable".into(), Value::Bool(pending));
    delta.changed = true;
}

/// 节点域名解析器迁移（issue #147）。上游 `migrateNodeResolver`。
/// nodeResolverMigrated!==true 时：nodeDomainResolver → nodeResolverPool/Single + 标记。
pub fn migrate_node_resolver(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(dns) = obj.get_mut("dnsConfig").and_then(|v| v.as_object_mut()) else {
        return;
    };
    if dns.get("nodeResolverMigrated") == Some(&Value::Bool(true)) {
        return;
    }
    // 克隆 old 为 owned String 以结束不可变借用，随后才可 mutable insert。
    let old = dns
        .get("nodeDomainResolver")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    if !dns.contains_key("nodeResolverPool") {
        let pool: Vec<&str> = match old.as_str() {
            "dnspod" => vec!["dnspod"],
            "system" => vec!["system"],
            _ => vec!["ali", "dnspod"],
        };
        dns.insert(
            "nodeResolverPool".into(),
            Value::Array(pool.into_iter().map(|s| Value::String(s.into())).collect()),
        );
    }
    if !dns.contains_key("nodeResolverSingle") {
        let single = match old.as_str() {
            "dnspod" => "dnspod",
            "system" => "system",
            _ => "ali",
        };
        dns.insert("nodeResolverSingle".into(), Value::String(single.into()));
    }
    dns.insert("nodeResolverMigrated".into(), Value::Bool(true));
    delta.changed = true;
}

/// 订阅代理策略迁移：旧布尔 subscriptionUpdateViaProxy → 三态 subscriptionProxyPolicy。
/// 上游 `migrateSubscriptionProxyPolicy`。仅旧字段存在时执行，消化后删旧字段。
pub fn migrate_subscription_proxy_policy(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if !obj.contains_key("subscriptionUpdateViaProxy") {
        return;
    }
    let legacy_true = obj
        .get("subscriptionUpdateViaProxy")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !obj.contains_key("subscriptionProxyPolicy") && legacy_true {
        obj.insert(
            "subscriptionProxyPolicy".into(),
            Value::String("proxy".into()),
        );
    }
    obj.remove("subscriptionUpdateViaProxy");
    delta.changed = true;
}

/// TUN stack 一次性迁移：存量 stack → 'auto' + tunStackMigrated 标记。上游 `migrateTunStackConfig`。
/// tunStackMigrated===true 即不动。
pub fn migrate_tun_stack(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.get("tunStackMigrated") == Some(&Value::Bool(true)) {
        return;
    }
    if let Some(Value::Object(tun)) = obj.get_mut("tunConfig") {
        tun.insert("stack".into(), Value::String("auto".into()));
    }
    obj.insert("tunStackMigrated".into(), Value::Bool(true));
    delta.changed = true;
}

/// TUN MTU 一次性迁移：抹掉存量 `tunConfig.mtu` → 缺席（= 自动）+ `tunMtuMigrated` 标记。
///
/// # 为什么可以整个抹掉，而不是「只抹掉等于旧默认的值」
///
/// **本项在此之前从未有过 UI 入口**（`SettingsTun` 只暴露 stack / autoRoute / strictRoute），故磁盘上
/// 的任何 `mtu` 都是程序写的默认值（新装 `default_config` 的 1350/1400，或旧 builder 的哨兵 9000），
/// 没有一个承载用户意图。逐值判断反而更糟：既要枚举历史默认（1350 / 1400 / 9000 / 未来还会有），
/// 又会把「碰巧手改成 1350」误当默认——而那种手改本来就无从与默认区分。一刀切既准确又不会漂。
///
/// 本迁移**只跑一次**：跑完置 `tunMtuMigrated`，此后用户在新 UI 里设的值不再被碰。
pub fn migrate_tun_mtu(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.get("tunMtuMigrated") == Some(&Value::Bool(true)) {
        return;
    }
    if let Some(Value::Object(tun)) = obj.get_mut("tunConfig") {
        tun.remove("mtu");
    }
    obj.insert("tunMtuMigrated".into(), Value::Bool(true));
    delta.changed = true;
}

/// 托盘菜单预热最终默认值的一次性迁移。
///
/// 该开关曾在中间验收构建里以 `false` 为默认并被持久化，最终产品决策改为默认 `true` 后，普通的
/// `or_insert(true)` 无法纠正已经存在的 `false`。配置里没有可用的版本来源来区分「中间默认值」与
/// 「用户在中间构建里手动关闭」，因此这里按最终产品默认统一纠正一次，并写入独立标记。标记落定后，
/// 用户再显式关闭预热会被完整保留，不会在后续启动时反复改回。
pub fn migrate_tray_menu_warm_default(value: &mut Value, delta: &mut MigrationDelta) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.get("keepTrayMenuWarmDefaultMigrated") == Some(&Value::Bool(true)) {
        return;
    }
    obj.insert("keepTrayMenuWarm".into(), Value::Bool(true));
    obj.insert("keepTrayMenuWarmDefaultMigrated".into(), Value::Bool(true));
    delta.changed = true;
}

/// **撤掉的「诊断采集」机制留下的孤儿键清理**（本项无历史锚点，是本仓自己的机制被删后的收尾）。
///
/// # 为什么必须有这条腿
///
/// 旧机制「开始采集」做的是：把当前 `logLevel` 快照进 `diagnosticCapture.prevLogLevel`，再把
/// `logLevel` 拉到 `debug`；「结束采集 / 下次启动自愈」再还原回去。机制整体删除后，**在采集中升级的
/// 用户**磁盘上留下的正是这个中间态：`logLevel:"debug"` + 一个再也没有代码会读的 `diagnosticCapture`。
/// 不清理的话，这两件事都会永久留着 —— 用户被静默钉在 debug 级别（日志刷屏、写盘量翻倍），
/// 而那个键成为谁也解释不了的孤儿。
///
/// # 判据是「键在即迁移」，不设独立标记
///
/// 迁移完键就没了，天然幂等，再加一个 `diagnosticCaptureMigrated` 只会是第二个孤儿键。
///
/// # 还原级别的取值
///
/// 只接受本仓 `LogLevel` 的五个合法值；缺失 / 损坏 / 不认识 → 兜 `info`，**绝不留 `debug`**
/// （留 debug 等于迁移没做）。这与旧 `diagnostic_capture_end` 的兜底口径逐字一致。
pub fn migrate_diagnostic_capture(value: &mut Value, delta: &mut MigrationDelta) {
    const LEVELS: [&str; 5] = ["debug", "info", "warn", "error", "fatal"];
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(cap) = obj.remove("diagnosticCapture") else {
        return; // 无残留 → no-op（绝大多数配置走这条）
    };
    delta.changed = true;
    // `null` 在旧机制里就等于「未在采集」（`diagnostic_capture_active` 判的是「存在且非 null」）⇒
    // 此时 `logLevel` 是用户自己的选择，**不得**被还原逻辑顶掉；只把这个空壳键删掉。
    if cap.is_null() {
        return;
    }
    let restored = cap
        .get("prevLogLevel")
        .and_then(|v| v.as_str())
        .filter(|s| LEVELS.contains(s))
        .unwrap_or("info")
        .to_string();
    obj.insert("logLevel".into(), Value::String(restored));
}

/// 应用分流默认预设注入。Polaris appRulesSeeded 段 + `seedDefaultAppRules`。
/// !appRulesSeeded 时：补内置预设默认规则 + 剔除下线预设（apple/bilibili）残留 + 置标记。
pub fn seed_app_rules(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let seeded = obj
        .get("appRulesSeeded")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if seeded {
        return false;
    }
    let existing = obj
        .get("appRules")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let seeded_rules = seed_default_app_rules(&existing);
    obj.insert(
        "appRules".into(),
        Value::Array(seeded_rules.into_iter().map(Value::Object).collect()),
    );
    obj.insert("appRulesSeeded".into(), Value::Bool(true));
    true
}

/// seedDefaultAppRules 纯逻辑：剔除下线预设 + 保留 custom-* + 补内置预设默认规则。
/// 复用 config-engine 的内置预设清单（default_app_rules 单一真值）。
fn seed_default_app_rules(existing: &[Value]) -> Vec<Map<String, Value>> {
    use std::collections::HashSet;
    // 内置预设 id 清单（复用 config-engine 单一真值，避免与本crate 静态列表漂移）。
    let valid_ids: HashSet<String> =
        polaris_config_engine::user_config::app_rules_preset::default_app_rules()
            .into_iter()
            .map(|r| r.app_id)
            .collect();
    let mut kept: Vec<Map<String, Value>> = Vec::new();
    let mut have: HashSet<String> = HashSet::new();
    for r in existing {
        let Some(o) = r.as_object() else {
            continue;
        };
        let Some(app_id) = o.get("appId").and_then(|v| v.as_str()) else {
            continue;
        };
        // 保留内置预设 + 自定义（custom-*）；下线预设（apple/bilibili 等）残留 → 丢弃。
        if valid_ids.contains(app_id) || app_id.starts_with("custom-") {
            kept.push(o.clone());
            have.insert(app_id.to_string());
        }
    }
    // 补缺失内置预设的默认「代理·跟全局」规则。
    for id in &valid_ids {
        if !have.contains(id) {
            let mut m = Map::new();
            m.insert("appId".into(), Value::String(id.clone()));
            m.insert("action".into(), Value::String("proxy".into()));
            m.insert("enabled".into(), Value::Bool(true));
            kept.push(m);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_config() -> Value {
        json!({
            "proxyMode": "global",
            "proxyModeType": "tun",
            "logLevel": "info",
            "mixedPort": 7890,
            "tunConfig": {"mtu": 1350, "stack": "system", "autoRoute": true, "strictRoute": true}
        })
    }

    #[test]
    fn migrate_legacy_domain_rule_produces_new_rules() {
        let mut v = base_config();
        v["customRules"] = json!([
            {"id":"old1","domains":["*.google.com","geosite:google"],"ipCidr":["10.0.0.0/8"],"action":"proxy","enabled":true}
        ]);
        migrate_custom_rules_domain_rule(&mut v);
        let rules = v["customRules"].as_array().unwrap();
        // 拆为 domainSuffix + geosite + ipCidr 三条
        assert!(rules.len() >= 2);
        let types: Vec<&str> = rules.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert!(types.contains(&"domainSuffix"));
        assert!(types.contains(&"geosite"));
        assert!(types.contains(&"ipCidr"));
    }

    #[test]
    fn migrate_custom_rules_idempotent() {
        let mut v = base_config();
        v["customRules"] = json!([
            {"id":"old1","domains":["a.com"],"action":"proxy","enabled":true}
        ]);
        migrate_custom_rules_domain_rule(&mut v);
        let after_first = v["customRules"].clone();
        migrate_custom_rules_domain_rule(&mut v);
        assert_eq!(v["customRules"], after_first, "二次迁移不变");
    }

    #[test]
    fn migrate_all_runs_full_chain_idempotent() {
        // 旧格式配置：缺 dnsConfig、stack=system、未 migrated、bypassProcesses 非空、appRulesSeeded 缺
        let mut v = json!({
            "proxyMode": "smart",
            "proxyModeType": "TUN",
            "logLevel": "info",
            "tunConfig": {"mtu": 1350, "stack": "gvisor", "autoRoute": true, "strictRoute": true},
            "bypassProcesses": ["chrome", "chrome", "  "],
            "subscriptionUpdateViaProxy": true,
            "appRules": [{"appId":"apple","action":"proxy","enabled":true}]
        });
        let delta1 = migrate_all(&mut v);
        assert!(delta1.changed, "首次迁移有变更");
        // 验证迁移效果
        assert_eq!(v["tunStackMigrated"], json!(true));
        assert_eq!(v["tunConfig"]["stack"], json!("auto"));
        // MTU 一并抹掉（存量 1350 是程序写的默认，不是用户意图）→ 缺席 = 自动。
        assert_eq!(v["tunMtuMigrated"], json!(true));
        assert!(v["tunConfig"].get("mtu").is_none(), "存量 mtu 应被抹掉");
        assert_eq!(v["subscriptionProxyPolicy"], json!("proxy"));
        assert!(!v
            .as_object()
            .unwrap()
            .contains_key("subscriptionUpdateViaProxy"));
        assert_eq!(v["appRulesSeeded"], json!(true));
        // apple 已下线 → 剔除
        let app_rules = v["appRules"].as_array().unwrap();
        assert!(!app_rules.iter().any(|r| r["appId"] == "apple"));
        // bypassProcesses 已迁移为 processName 规则
        assert_eq!(v["bypassProcesses"], json!([]));
        let custom = v["customRules"].as_array().unwrap();
        assert!(custom
            .iter()
            .any(|r| r["id"] == "migrated_bypass_processes"));
        // dnsConfig 已补齐 + 标记
        assert_eq!(v["dnsConfig"]["fakeIpToggleMigrated"], json!(true));
        assert_eq!(v["dnsConfig"]["nodeResolverMigrated"], json!(true));

        // 二次跑：幂等，无变更（changed 应为 false）
        let snapshot = v.clone();
        let delta2 = migrate_all(&mut v);
        assert_eq!(v, snapshot, "二次迁移完全不变");
        assert!(!delta2.changed, "二次迁移标记无变更");
    }

    /// 🔴 「诊断采集」机制删除后的收尾：**在采集中升级**的用户不得被静默钉在 debug 级别。
    ///
    /// 那个中间态是磁盘上真实存在的形态（`logLevel:"debug"` + `diagnosticCapture.prevLogLevel`）。
    /// 少了这条迁移，用户升级后日志永远以 debug 刷屏、写盘量翻倍，而界面上再也没有任何一颗按钮
    /// 能关掉它 —— 关它的那颗按钮已经随机制一起删了。
    ///
    /// 牙：把 `migrate_diagnostic_capture` 从 `migrate_all` 链上摘掉 → 第一条断言转红；
    /// 把还原值改成保留 `debug` → 第二条转红；把 null 那条早返删掉 → 第四条转红。
    #[test]
    fn migrate_diagnostic_capture_restores_level_and_drops_orphan_key() {
        // ① 真实中间态：采集中升级。
        let mut v = base_config();
        v["logLevel"] = json!("debug");
        v["diagnosticCapture"] = json!({ "prevLogLevel": "warn" });
        let delta = migrate_all(&mut v);
        assert!(delta.changed);
        assert!(
            v.as_object().unwrap().get("diagnosticCapture").is_none(),
            "孤儿键必须清掉（再也没有代码读它）"
        );
        assert_eq!(
            v["logLevel"],
            json!("warn"),
            "必须还原到采集前的级别，不得把用户钉在 debug"
        );

        // ② 快照损坏 / 缺失 → 兜 info，绝不留 debug。
        let mut v = base_config();
        v["logLevel"] = json!("debug");
        v["diagnosticCapture"] = json!({});
        migrate_all(&mut v);
        assert_eq!(v["logLevel"], json!("info"), "缺快照兜 info");

        let mut v = base_config();
        v["logLevel"] = json!("debug");
        v["diagnosticCapture"] = json!({ "prevLogLevel": "verbose" });
        migrate_all(&mut v);
        assert_eq!(v["logLevel"], json!("info"), "不认识的级别兜 info");

        // ③ null 在旧机制里就等于「未在采集」⇒ 只删空壳键，logLevel 是用户自己的选择，不得被顶掉。
        let mut v = base_config();
        v["logLevel"] = json!("error");
        v["diagnosticCapture"] = Value::Null;
        migrate_all(&mut v);
        assert!(v.as_object().unwrap().get("diagnosticCapture").is_none());
        assert_eq!(
            v["logLevel"],
            json!("error"),
            "null = 未在采集，不得把用户选的级别改掉"
        );

        // ④ 无该键 → 完全不碰 logLevel（绝大多数配置走这条；碰了就是每次启动改用户级别）。
        let mut v = base_config();
        v["logLevel"] = json!("error");
        migrate_all(&mut v);
        assert_eq!(v["logLevel"], json!("error"));

        // ⑤ 幂等：迁移完键就没了，二次跑无变更。
        let mut v = base_config();
        v["logLevel"] = json!("debug");
        v["diagnosticCapture"] = json!({ "prevLogLevel": "warn" });
        migrate_all(&mut v);
        let snapshot = v.clone();
        let delta2 = migrate_all(&mut v);
        assert_eq!(v, snapshot);
        assert!(!delta2.changed);
    }

    /// 🔴 MTU 迁移**只跑一次**：标记落定后，用户在新 UI 里设的值不得再被抹。
    ///
    /// 没有这条锁，`migrate_tun_mtu` 退化成「每次启动都把 MTU 清成自动」——用户手设一个值、
    /// 重启一次就没了，且没有任何提示。这是本迁移唯一的危险失效模式。
    #[test]
    fn tun_mtu_migration_runs_once_then_respects_user_value() {
        let mut v = json!({
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        let mut d = MigrationDelta::default();
        migrate_tun_mtu(&mut v, &mut d);
        assert!(d.changed);
        assert!(v["tunConfig"].get("mtu").is_none(), "存量值应被抹掉");
        assert_eq!(v["tunMtuMigrated"], json!(true));

        // 用户之后在 UI 里显式设了 65535 —— 再跑迁移必须原样保留。
        v["tunConfig"]["mtu"] = json!(65535);
        let mut d2 = MigrationDelta::default();
        migrate_tun_mtu(&mut v, &mut d2);
        assert_eq!(
            v["tunConfig"]["mtu"],
            json!(65535),
            "已迁移后不得再碰用户值"
        );
        assert!(!d2.changed);
    }

    /// 中间构建把默认 false 持久化后，最终默认必须只纠正一次；随后用户仍可独立关闭。
    #[test]
    fn tray_menu_warm_default_migration_runs_once_then_respects_user_value() {
        let mut v = json!({"keepTrayMenuWarm": false});
        let mut d = MigrationDelta::default();
        migrate_tray_menu_warm_default(&mut v, &mut d);
        assert!(d.changed);
        assert_eq!(v["keepTrayMenuWarm"], json!(true));
        assert_eq!(v["keepTrayMenuWarmDefaultMigrated"], json!(true));

        // 迁移完成后用户显式关闭，二次启动不得再覆写。
        v["keepTrayMenuWarm"] = json!(false);
        let mut d2 = MigrationDelta::default();
        migrate_tray_menu_warm_default(&mut v, &mut d2);
        assert_eq!(v["keepTrayMenuWarm"], json!(false));
        assert!(!d2.changed);
    }

    #[test]
    fn tray_menu_warm_default_migration_fills_missing_value_and_is_idempotent() {
        let mut v = json!({});
        let mut d = MigrationDelta::default();
        migrate_tray_menu_warm_default(&mut v, &mut d);
        assert!(d.changed);
        assert_eq!(v["keepTrayMenuWarm"], json!(true));
        assert_eq!(v["keepTrayMenuWarmDefaultMigrated"], json!(true));

        let snapshot = v.clone();
        let mut d2 = MigrationDelta::default();
        migrate_tray_menu_warm_default(&mut v, &mut d2);
        assert_eq!(v, snapshot);
        assert!(!d2.changed);
    }

    #[test]
    fn migrate_bypass_processes_dedupes() {
        let mut v = base_config();
        v["bypassProcesses"] = json!(["chrome", "chrome", "firefox", "  "]);
        migrate_bypass_processes(&mut v);
        let rule = v["customRules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "migrated_bypass_processes")
            .unwrap();
        let vals = rule["values"].as_array().unwrap();
        assert_eq!(vals.len(), 2); // chrome + firefox 去重
    }

    #[test]
    fn migrate_subscription_proxy_policy_false_leaves_default() {
        let mut v = base_config();
        v["subscriptionUpdateViaProxy"] = json!(false);
        migrate_subscription_proxy_policy(&mut v, &mut MigrationDelta::default());
        // false → 不设 policy（回落 follow 默认），但旧字段已删
        assert!(!v
            .as_object()
            .unwrap()
            .contains_key("subscriptionProxyPolicy"));
        assert!(!v
            .as_object()
            .unwrap()
            .contains_key("subscriptionUpdateViaProxy"));
    }

    #[test]
    fn migrate_node_resolver_preserves_intent() {
        let mut v = base_config();
        v["proxyModeType"] = json!("systemProxy");
        v["dnsConfig"] = json!({"nodeDomainResolver": "dnspod"});
        migrate_node_resolver(&mut v, &mut MigrationDelta::default());
        assert_eq!(v["dnsConfig"]["nodeResolverPool"], json!(["dnspod"]));
        assert_eq!(v["dnsConfig"]["nodeResolverSingle"], json!("dnspod"));
        assert_eq!(v["dnsConfig"]["nodeResolverMigrated"], json!(true));
    }

    #[test]
    fn custom_rules_need_migration_detects_legacy_only() {
        // 旧 DomainRule（无 type + domains 数组）→ 需迁移。
        let legacy = json!({"customRules":[{"id":"a","domains":["x.com"],"action":"proxy"}]});
        assert!(custom_rules_need_migration(&legacy));
        // 新 Rule（有 type）→ 不需迁移。
        let modern = json!({"customRules":[{"id":"a","type":"domainSuffix","values":["x.com"],"action":"proxy","enabled":true}]});
        assert!(!custom_rules_need_migration(&modern));
        // 空 / 缺失 → 不需迁移。
        assert!(!custom_rules_need_migration(&json!({"customRules":[]})));
        assert!(!custom_rules_need_migration(&json!({})));
    }

    #[test]
    fn migrate_privacy_password_clears_plaintext() {
        // F29：明文 privacyPassword → 内存清空（防外泄）；哈希落盘属运行时层 FS。
        let mut v = base_config();
        v["privacyPassword"] = json!("secret123");
        let changed = migrate_privacy_password_clear(&mut v);
        assert!(changed);
        assert_eq!(v["privacyPassword"], json!(""));
        // 幂等：已空不再变
        let changed2 = migrate_privacy_password_clear(&mut v);
        assert!(!changed2);
    }
}
