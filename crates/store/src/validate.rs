//! 配置语义校验（validateConfig 中会 throw 的字段）+ 纯校验辅助函数。
//!
//! Polaris 锚点：`ConfigManager.ts#validateConfig` 中 throw 的分支（proxyMode/proxyModeType/
//! logLevel/tunConfig 必填/端口范围/布尔必填），以及复用的纯校验单一真值：
//!   - `shared/server-completeness#ALL_PROTOCOLS` / `protocolRequirementError`
//!   - `shared/rules#RULE_TYPE_IDS` / `isValidIpCidr` / `validateRuleValue`
//!   - `shared/tun-stack#TUN_STACK_VALUES`
//!   - `shared/direct-selection#isDirectSelection`
//!
//! 分层：[`sanitize_config`] 先做形状清洗（坏字段删除/退化），
//! 本模块对「形状合法但语义非法」的字段做最终判定（缺失必填 / 范围越界 / 枚举非法）→ Err。
//! 错误交 loadConfig catch（已备份不覆盖）。
//!
//! 纯逻辑：本模块不触碰 FS，仅接受 [`serde_json::Value`]（sanitize 的产物）。

#![forbid(unsafe_code)]

use serde_json::Value;

// ── 规则值/规则聚合校验：单一真值在 config-engine `user_config::rule_validate` ──
// 历史上 `store/validate.rs` 与 `config-engine/user_config/rules.rs` 各持一份规则校验（含各自私有的
// `valid_port_token`）。D4 收敛为一份：完整实现落 config-engine（持有 `Rule`/`RuleType` 类型的最底层
// crate），本模块 re-export 之——保持 `crate::validate::{is_valid_ip_cidr, is_known_rule_type, ...}`
// 调用点不变（sanitize 复用）。`is_valid_ip_cidr` 亦供下方 `normalize_tun_exclude_cidr` 复用。
pub use polaris_config_engine::user_config::{
    is_known_rule_type, is_valid_ip_cidr, validate_rule_value, RULE_TYPE_IDS,
};

/// 受支持协议清单（上游 `ALL_PROTOCOLS`）。大小写不敏感匹配（validate 时 toLowerCase）。
pub const ALLOWED_PROTOCOLS: &[&str] = &[
    "vless",
    "vmess",
    "trojan",
    "hysteria2",
    "shadowsocks",
    "anytls",
    "tuic",
    "naive",
    "snell",
    "socks",
    "http",
    "ssh",
    "wireguard",
    "tailscale",
    // 2026-08-11：随包核支持而此前无表单的两个出站。
    // shadowtls 不在此列且非遗漏——它是 shadowsocks 的插件设置，生成侧自动造外层出站接 detour。
    "hysteria",
    "tor",
    // 端点族 VPN 客户端（进 endpoints[]，但语义是普通出口、不是组网）。
    "openconnect",
    "openvpn-client",
    "custom",
];

/// 协议是否受支持（小写比对）。
pub fn is_allowed_protocol(proto_lower: &str) -> bool {
    ALLOWED_PROTOCOLS.contains(&proto_lower)
}

/// TUN stack 合法值（含 auto）。上游 `TUN_STACK_VALUES`。
pub const TUN_STACK_VALUES: &[&str] = &["auto", "system", "gvisor", "mixed"];

/// 全局直连哨兵（上游 `DIRECT_SERVER_ID`）。selectedServerId 取此值 = 全局直连，豁免存在性校验。
pub const DIRECT_SERVER_ID: &str = "__direct__";

/// 是否直连选择（哨兵）。
pub fn is_direct_selection(id: &str) -> bool {
    id == DIRECT_SERVER_ID
}

// ── 规范化 TUN 排除 CIDR：单一真值在 config-engine `builder::tun_route_exclude` ──
// 与上面规则校验同理，这里曾另有一份逐行等价的实现（同样的裸 IP 补掩码 / 同样的 v4<8·v6<7 过宽阈值）。
// 两份实现今天同口径，但**没有任何护栏**阻止它们漂移：sanitize 用这份决定「什么能落盘」，
// `compute_user_tun_exclude` 用那份决定「什么能进 route_exclude_address」——一旦阈值只改一边，
// 就会出现「存得进配置、生成时被静默剔掉」（或反之）的错配，而两处各自的单测都还是绿的。
// 收敛为 re-export：调用点路径 `crate::validate::normalize_tun_exclude_cidr` 保持不变（sanitize 复用）。
pub use polaris_config_engine::builder::tun_route_exclude::normalize_tun_exclude_cidr;

/// 协议必填字段是否齐备（上游 `protocolRequirementError`，返回 null=OK）。
/// `server` 为 serde_json::Value（sanitize 产物）。
pub fn protocol_requirement_ok(proto_lower: &str, server: &Value) -> bool {
    let obj = match server.as_object() {
        Some(o) => o,
        None => return false,
    };
    let nonempty = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    };
    match proto_lower {
        "vless" | "vmess" => nonempty("uuid"),
        "trojan" | "hysteria2" | "anytls" => nonempty("password"),
        "tuic" => nonempty("uuid") && nonempty("password"),
        "naive" => nonempty("username") && nonempty("password"),
        "snell" => {
            nonempty("password")
                && obj
                    .get("snellSettings")
                    .and_then(|s| s.get("version"))
                    .and_then(|v| v.as_u64())
                    .is_some_and(|ver| ver == 4 || ver == 6)
        }
        "shadowsocks" => {
            obj.get("shadowsocksSettings")
                .and_then(|s| s.get("method"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty())
                && obj
                    .get("shadowsocksSettings")
                    .and_then(|s| s.get("password"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
        }
        "wireguard" => {
            let wg = obj.get("wireguardSettings");
            wg.and_then(|s| s.get("privateKey"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty())
                && wg
                    .and_then(|s| s.get("peerPublicKey"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
                && wg
                    .and_then(|s| s.get("localAddress"))
                    .and_then(|v| v.as_array())
                    .is_some_and(|a| !a.is_empty())
        }
        "socks" | "http" | "ssh" | "tailscale" => true, // 仅需 address/port（通用校验）或账号制无硬必填
        "custom" => {
            // raw-JSON 透传：须是含 type 字段的 outbound 对象
            obj.get("customSettings")
                .and_then(|s| s.get("outbound"))
                .and_then(|o| o.get("type"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.trim().is_empty())
        }
        _ => false,
    }
}

/// 对 sanitize 后的 Value 做最终语义校验（就地归一 proxyModeType 为 camelCase 规范值）。
///
/// Polaris 锚点 validateConfig 中 throw 的分支：proxyMode/proxyModeType/logLevel 枚举、
/// tunConfig 必填 + mtu 范围 + stack 合法、端口范围、布尔必填。
/// sanitize 已删除类型错的字段，这里对「缺失必填 / 枚举非法 / 范围越界」判 Err。
///
/// proxyModeType 归一：validateConfig 原行为回写规范 camelCase（systemProxy/tun/manual），
/// 使磁盘值恒为规范形 → 全栈精确比较（=== 'systemProxy'）可信。接收 &mut Value 以便回写。
pub fn validate_config(value: &mut Value) -> Result<(), crate::StoreError> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| crate::StoreError::validation("config root must be an object"))?;

    // proxyMode（global/smart/direct，大小写不敏感）
    let mode = obj.get("proxyMode").and_then(|v| v.as_str()).unwrap_or("");
    let mode_lower = mode.to_ascii_lowercase();
    if !matches!(mode_lower.as_str(), "global" | "smart" | "direct") {
        return Err(crate::StoreError::validation(
            "proxyMode must be global, smart, or direct",
        ));
    }

    // proxyModeType（systemProxy/tun/manual，大小写不敏感）→ 归一 camelCase
    let mtype = obj
        .get("proxyModeType")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mtype_lower = mtype.to_ascii_lowercase();
    let canonical = match mtype_lower.as_str() {
        "systemproxy" => Some("systemProxy"),
        "tun" => Some("tun"),
        "manual" => Some("manual"),
        _ => None,
    };
    let canonical = canonical.ok_or_else(|| {
        crate::StoreError::validation("proxyModeType must be systemProxy, tun, or manual")
    })?;

    // 回写规范 camelCase（validateConfig 原行为）：磁盘值恒为规范形，全栈 === 比较可信。
    obj.insert("proxyModeType".into(), Value::String(canonical.into()));

    // tunConfig 必填 + 字段校验
    let tun = obj
        .get("tunConfig")
        .ok_or_else(|| crate::StoreError::validation("tunConfig is required"))?;
    let tun = tun
        .as_object()
        .ok_or_else(|| crate::StoreError::validation("tunConfig is required"))?;
    // mtu **缺席即合法**（= 自动，按最终栈 × 平台派生，见 config-engine `tun_stack::default_mtu_for`）。
    // 在场则必须是 1280–65535 的数；`null` / 字符串 / 越界一律拒——「设了但是脏值」与「没设」是两回事，
    // 前者静默吞掉就是又一个「设置了不生效」。
    if let Some(raw) = tun.get("mtu").filter(|v| !v.is_null()) {
        let mtu = raw.as_u64().filter(|m| (1280..=65535).contains(m));
        if mtu.is_none() {
            return Err(crate::StoreError::validation(
                "tunConfig.mtu must be a number between 1280 and 65535",
            ));
        }
    }
    let stack = tun.get("stack").and_then(|v| v.as_str()).unwrap_or("");
    if !TUN_STACK_VALUES.contains(&stack) {
        return Err(crate::StoreError::validation(
            "tunConfig.stack must be auto, system, gvisor, or mixed",
        ));
    }
    if !tun.get("autoRoute").is_some_and(|v| v.is_boolean()) {
        return Err(crate::StoreError::validation(
            "tunConfig.autoRoute must be a boolean",
        ));
    }
    if !tun.get("strictRoute").is_some_and(|v| v.is_boolean()) {
        return Err(crate::StoreError::validation(
            "tunConfig.strictRoute must be a boolean",
        ));
    }

    // 端口范围（sanitize 已保证类型；这里校验范围）
    validate_port(obj, "mixedPort", true)?;
    validate_port(obj, "controlPort", false)?;

    // controlPort 默认填充 + 撞口自动避让（上游 validateConfig L380-389 逐字移植）。
    // 未设(>0) → 9090（DEFAULT_CONTROL_PORT）；与 mixedPort 同口 → 回退（9090 时取 9091，否则取 9090），
    // 杜绝 clash_api external_controller 与 mixed inbound 撞口致 sing-box FATAL。此校验护 loadConfig /
    // saveConfig（含导入 backup_import_apply）两路径——它们都过本函数——故手改/导入的撞口在落盘前即被纠正。
    // sanitize 已删非正整数端口，故此处 controlPort 要么缺失、要么 ∈[1,65535]（上方 validate_port 已校范围）。
    if let Some(mixed) = obj.get("mixedPort").and_then(Value::as_u64) {
        let control = obj
            .get("controlPort")
            .and_then(Value::as_u64)
            .filter(|&p| p >= 1)
            .unwrap_or(9090);
        let control = if control == mixed {
            if mixed == 9090 {
                9091
            } else {
                9090
            }
        } else {
            control
        };
        obj.insert("controlPort".into(), Value::from(control));
    }

    // logLevel（debug/info/warn/error/fatal）
    let level = obj.get("logLevel").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(level, "debug" | "info" | "warn" | "error" | "fatal") {
        return Err(crate::StoreError::validation(
            "logLevel must be debug, info, warn, error, or fatal",
        ));
    }

    Ok(())
}

/// 校验端口字段范围。required=true 时缺失/非法 → Err。
fn validate_port(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    required: bool,
) -> Result<(), crate::StoreError> {
    match obj.get(key).and_then(|v| v.as_u64()) {
        Some(p) if (1..=65535).contains(&p) => Ok(()),
        Some(_) => Err(crate::StoreError::validation(format!(
            "{key} must be a number between 1 and 65535"
        ))),
        None if required => Err(crate::StoreError::validation(format!(
            "{key} must be a number between 1 and 65535"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_cidr_reexport_is_strict() {
        // re-export 的 is_valid_ip_cidr 仍是 config-engine 的严格实现（sanitize/normalize 依赖此语义）。
        assert!(is_valid_ip_cidr("10.0.0.0/8"));
        assert!(!is_valid_ip_cidr("010.0.0.1")); // 前导零
        assert!(!is_valid_ip_cidr("10.0.0.0/40")); // 掩码超界
    }

    #[test]
    fn normalize_tun_exclude() {
        assert_eq!(
            normalize_tun_exclude_cidr("10.0.0.0/8"),
            Some("10.0.0.0/8".into())
        );
        assert_eq!(
            normalize_tun_exclude_cidr("1.2.3.4"),
            Some("1.2.3.4/32".into())
        );
        assert_eq!(normalize_tun_exclude_cidr("::1"), Some("::1/128".into()));
        // 过宽（v4<8）
        assert_eq!(normalize_tun_exclude_cidr("0.0.0.0/0"), None);
        assert_eq!(normalize_tun_exclude_cidr("10.0.0.0/7"), None);
        // 非法
        assert_eq!(normalize_tun_exclude_cidr("not-a-cidr"), None);
        assert_eq!(normalize_tun_exclude_cidr(""), None);
    }

    #[test]
    fn protocol_requirement_checks() {
        // trojan 缺 password → 不通过
        let bad = serde_json::json!({"id":"s","name":"S","protocol":"trojan","address":"1.2.3.4","port":443});
        assert!(!protocol_requirement_ok("trojan", &bad));
        let good = serde_json::json!({"id":"s","name":"S","protocol":"trojan","password":"pw","address":"1.2.3.4","port":443});
        assert!(protocol_requirement_ok("trojan", &good));
        // wireguard 缺 localAddress → 不通过
        let wg = serde_json::json!({"wireguardSettings":{"privateKey":"k","peerPublicKey":"p"}});
        assert!(!protocol_requirement_ok("wireguard", &wg));
    }

    #[test]
    fn validate_accepts_minimal_config() {
        let mut v = serde_json::json!({
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 7890,
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        assert!(validate_config(&mut v).is_ok());
    }

    #[test]
    fn validate_rejects_bad_proxy_mode() {
        let mut v = serde_json::json!({
            "proxyMode": "invalid",
            "proxyModeType": "tun",
            "logLevel": "info",
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        assert!(matches!(
            validate_config(&mut v),
            Err(crate::StoreError::Validation(_))
        ));
    }

    #[test]
    fn control_port_defaults_to_9090_when_missing() {
        // 缺 controlPort → 填 9090（上游 `if (!controlPort) controlPort=9090`）。
        let mut v = serde_json::json!({
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 7890,
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        validate_config(&mut v).unwrap();
        assert_eq!(v["controlPort"], serde_json::json!(9090));
    }

    #[test]
    fn control_port_collision_with_mixed_reassigns() {
        // controlPort == mixedPort（非 9090）→ 回退 9090（撞口自愈，否则 sing-box FATAL）。
        let mut v = serde_json::json!({
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 7890,
            "controlPort": 7890,
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        validate_config(&mut v).unwrap();
        assert_eq!(v["controlPort"], serde_json::json!(9090));
        assert_ne!(v["controlPort"], v["mixedPort"], "撞口必须被拆开");
    }

    #[test]
    fn control_port_collision_at_9090_reassigns_to_9091() {
        // 两口都 9090 → controlPort 取 9091（上游 `mixedPort===9090 ? 9091 : 9090`）。
        let mut v = serde_json::json!({
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 9090,
            "controlPort": 9090,
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        validate_config(&mut v).unwrap();
        assert_eq!(v["controlPort"], serde_json::json!(9091));
        assert_ne!(v["controlPort"], v["mixedPort"]);
    }

    #[test]
    fn control_port_non_colliding_preserved() {
        // 不撞口 → 原值保留（幂等，不乱改用户设的端口）。
        let mut v = serde_json::json!({
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 8080,
            "controlPort": 9091,
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        validate_config(&mut v).unwrap();
        assert_eq!(v["controlPort"], serde_json::json!(9091));
    }

    #[test]
    fn validate_rejects_bad_mtu() {
        let mut v = serde_json::json!({
            "proxyMode": "direct",
            "proxyModeType": "manual",
            "logLevel": "info",
            "tunConfig": {"mtu": 100, "stack": "auto", "autoRoute": true, "strictRoute": true}
        });
        assert!(validate_config(&mut v).is_err());
    }
}
