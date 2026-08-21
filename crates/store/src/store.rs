//! ConfigStore trait + ConfigManager 纯逻辑核心。
//!
//! Polaris 锚点：`ConfigManager.ts`（loadConfig / saveConfig / get / set 全量移植）。
//!
//! 分层：
//! - [`ConfigStore`]：编排 load（sanitize → migrate → validate → defaults）与 save
//!   （validate → atomic_write）的纯逻辑函数，FS 经 [`ConfigFs`] trait 注入。
//! - [`ConfigStore`] 不持有 currentConfig 缓存（那是运行时层的事；纯逻辑无状态）。
//!
//! 纪律：
//! - sanitize-don't-throw（维度7 #7）：坏 JSON/坏字段 → 内存回落默认配置，**磁盘真实文件原样保留**
//!   （仅 ENOENT 新装才落盘默认）。
//! - 迁移链幂等 + 绝不抛（维度7 #54）：迁移落盘失败不阻断（best-effort，吞错误）。
//! - 原子写：tmp→rename，防半写截断 → 默认覆盖 → 配置全丢。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::fs::{atomic_write_plan, AtomicWritePlan, ConfigFs};
use crate::migrate::{migrate_all, MigrationDelta};
use crate::{sanitize_config, validate_config, StoreError};

/// 加载结果。
#[derive(Debug, Clone)]
pub struct LoadResult {
    /// 内存生效配置（sanitize + migrate + validate 后；失败则默认配置）。
    pub config: Value,
    /// 加载是否从磁盘成功读取并解析（false = 回落默认）。
    pub loaded_from_disk: bool,
    /// 迁移是否有变更（调用方据此决定是否落盘）。
    pub migration_delta: MigrationDelta,
    /// 文件原本不存在（ENOENT）→ 新装，可安全落盘默认配置。
    pub was_missing: bool,
    /// 加载/校验错误（若 loaded_from_disk=false，此处是回落原因；用于日志）。
    pub error: Option<StoreError>,
}

/// 配置存储核心（纯逻辑 + trait FS 注入）。
///
/// 无状态：每次 load/save 经 trait 读写磁盘。currentConfig 缓存由运行时层（src-tauri）
/// 在本核心之上包装维护（对齐 Polaris ConfigManager.currentConfig）。
pub struct ConfigStore;

impl ConfigStore {
    /// 加载配置：read → sanitize → migrate → validate → 填默认。
    ///
    /// 维度7 #7：坏 JSON/坏字段绝不崩溃，回落默认配置；**磁盘真实文件绝不覆盖**
    /// （仅 was_missing=true 新装才允许落盘默认）。
    /// 维度7 #54：迁移链全量执行（幂等 + 吞异常）。
    pub fn load<F: ConfigFs + ?Sized>(fs: &F, path: &Path) -> LoadResult {
        let path_buf: PathBuf = path.to_path_buf();
        if !fs.exists(&path_buf) {
            // 新装：返回默认配置，标记 was_missing 供调用方落盘。
            return LoadResult {
                config: default_config(),
                loaded_from_disk: false,
                migration_delta: MigrationDelta::default(),
                was_missing: true,
                error: None,
            };
        }
        // 「存在但加载失败」的公共兜底：备份损坏文件（config.corrupt-<ts>.json，copy 不覆盖原文件）+ 保留最近 2 份，
        // 再回落默认配置。Polaris loadConfig catch 的非-ENOENT 分支——否则损坏配置首次改设置即被默认静默覆盖、永久丢失。
        let fallback_corrupt = |error: StoreError| -> LoadResult {
            Self::backup_corrupt(fs, &path_buf, &corrupt_backup_stamp());
            prune_corrupt_backups(fs, &path_buf);
            LoadResult {
                config: default_config(),
                loaded_from_disk: false,
                migration_delta: MigrationDelta::default(),
                was_missing: false,
                error: Some(error),
            }
        };
        let content = match fs.read_to_string(&path_buf) {
            Ok(c) => c,
            // 读取失败（权限/IO）→ 备份（best-effort，copy 亦可能失败被吞）+ 回落默认，不覆盖磁盘。
            Err(e) => return fallback_corrupt(e),
        };
        // 旧规则（DomainRule）→ 新规则（Rule）迁移前先备份原配置（仅首次，便于回滚踩边界 bug）。
        // Polaris loadConfig：customRulesNeedMigration && !exists(.pre-rule-migration.bak) → copyFile。best-effort。
        // 判据须看**原始 JSON**（未过 sanitize）——`sanitize_custom_rules` 会丢弃无 `type` 字段的条目（旧
        // DomainRule 恰无 type），若在 sanitize 后判会永假、备份形同虚设。轻量二次 parse（config 体量小，非热路径）。
        if serde_json::from_str::<Value>(&content)
            .ok()
            .is_some_and(|raw| crate::migrate::custom_rules_need_migration(&raw))
        {
            let bak = path_buf.with_file_name(format!(
                "{}.pre-rule-migration.bak",
                path_buf
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            ));
            if !fs.exists(&bak) {
                let _ = fs.copy(&path_buf, &bak);
            }
        }
        // sanitize（宽容反序列化，坏字段跳过）
        let mut value = match sanitize_config(&content) {
            Ok(v) => v,
            // 坏 JSON / 顶层非对象 → 备份损坏文件 + 回落默认，不覆盖磁盘。
            Err(e) => return fallback_corrupt(e),
        };
        // 迁移链（全量，幂等，绝不抛）
        let migration_delta = migrate_all(&mut value);
        // validate（语义校验：必填/枚举/范围）+ 填默认
        if let Err(e) = finalize_config(&mut value) {
            // 校验失败 → 备份损坏文件 + 回落默认，不覆盖磁盘。
            return fallback_corrupt(e);
        }
        LoadResult {
            config: value,
            loaded_from_disk: true,
            migration_delta,
            was_missing: false,
            error: None,
        }
    }

    /// 保存配置：validate → 原子写（tmp→rename）。
    ///
    /// 维度7 #7：保存前复跑 sanitize-shape + validate，确保落盘的永远是合法配置。
    /// 原子写防半写截断 → loadConfig 校验失败 → 默认覆盖 → 配置全丢。
    pub fn save<F: ConfigFs + ?Sized>(
        fs: &F,
        path: &Path,
        config: &Value,
        suffix_hex: &str,
    ) -> Result<(), StoreError> {
        // 保存前深拷贝并复跑 sanitize + validate（saveConfig 经 validateConfig 同口径）。
        let mut value = config.clone();
        crate::sanitize::sanitize_value_in_place_pub(&mut value);
        validate_config(&mut value)?;
        let content = serde_json::to_string_pretty(&value).map_err(StoreError::from)?;
        let plan: AtomicWritePlan = atomic_write_plan(path, suffix_hex, &content);
        plan.execute(fs)
    }

    /// 备份损坏配置（best-effort，绝不阻断）。
    /// Polaris loadConfig catch：配置损坏 → copy 到 config.corrupt-<ts>.json，不覆盖原文件。
    pub fn backup_corrupt<F: ConfigFs + ?Sized>(fs: &F, path: &Path, stamp: &str) {
        let backup = path.with_file_name(format!("config.corrupt-{stamp}.json"));
        let _ = fs.copy(path, &backup);
    }
}

/// 损坏配置备份用的文件系统安全时间戳（供 [`ConfigStore::load`] 失败分支）。
///
/// 上游 `corruptBackupStamp`：`new Date().toISOString().replace(/[:.]/g, '-')`（去冒号/点防 Windows 非法名）。
/// 本 workspace 无 chrono/time 依赖（最小依赖面），故用 UNIX 纪元 `秒-纳秒` 定宽零填充：**字典序 == 时间序**
/// （[`prune_corrupt_backups`] 靠文件名排序取最近，故必须可排序），且比秒级更细避免同秒备份互相覆盖。
#[must_use]
pub fn corrupt_backup_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // 秒定宽 14 位（够到公元 500 万年）+ 纳秒 9 位 → 恒等长、字典序即时间序。
    format!("{:014}-{:09}", now.as_secs(), now.subsec_nanos())
}

/// 清理 `config.corrupt-*.json` 备份，仅保留最近 2 份（best-effort，绝不阻断）。
///
/// 上游 `pruneCorruptBackups`：反复启动失败时防备份在 userData 无限堆积。时间戳前缀命名 → 字典序即时间序
/// （升序，旧在前）；删除除末尾 2 份外的所有。任何 FS 失败仅忽略。
pub fn prune_corrupt_backups<F: ConfigFs + ?Sized>(fs: &F, config_path: &Path) {
    let Some(dir) = config_path.parent() else {
        return;
    };
    let Ok(entries) = fs.list_dir(dir) else {
        return;
    };
    let mut backups: Vec<String> = entries
        .into_iter()
        .filter(|n| n.starts_with("config.corrupt-") && n.ends_with(".json"))
        .collect();
    backups.sort(); // 字典序 == 时间序（升序，旧在前）
    let keep = 2;
    if backups.len() <= keep {
        return;
    }
    for name in &backups[..backups.len() - keep] {
        let _ = fs.remove(&dir.join(name));
    }
}

/// finalize：validate + 填默认字段（logLevel/mixedPort 等 sanitize 未回填的必填）。
///
/// Polaris validateConfig 末段：缺 logLevel → throw（这里 sanitize 已保证形状，
/// validate 判枚举）；mixedPort 缺 → migrate_mixed_port 已填；proxyMode/proxyModeType
/// 缺 → 这里填默认（与 createDefaultConfig 一致）。
pub fn finalize_config(value: &mut Value) -> Result<(), StoreError> {
    // 必填默认填充（validateConfig 各分支的「未定义则设默认」）。限制在作用域内，结束后释放借用。
    {
        let Some(obj) = value.as_object_mut() else {
            return Err(StoreError::validation("config root must be an object"));
        };
        obj.entry("proxyMode")
            .or_insert_with(|| Value::String("smart".into()));
        obj.entry("proxyModeType")
            .or_insert_with(|| Value::String("systemProxy".into()));
        obj.entry("logLevel")
            .or_insert_with(|| Value::String("info".into()));
        obj.entry("mixedPort")
            .or_insert_with(|| Value::from(7890u16));
        // tunConfig 缺 → validate 会判 Err（必填）；这里不补（保持 Polaris 行为：缺 tunConfig throw）。
        // bool 必填默认
        obj.entry("autoStart").or_insert_with(|| Value::Bool(false));
        obj.entry("autoConnect")
            .or_insert_with(|| Value::Bool(false));
        obj.entry("minimizeToTray")
            .or_insert_with(|| Value::Bool(true));
        obj.entry("silentStart")
            .or_insert_with(|| Value::Bool(false));
        obj.entry("autoCheckUpdate")
            .or_insert_with(|| Value::Bool(true));
        obj.entry("autoLightweightMode")
            .or_insert_with(|| Value::Bool(false));
        obj.entry("keepTrayMenuWarm")
            .or_insert_with(|| Value::Bool(true));
        obj.entry("rememberWindowSize")
            .or_insert_with(|| Value::Bool(true));
        obj.entry("interruptConnectionsOnSwitch")
            .or_insert_with(|| Value::Bool(true));
        obj.entry("bypassProcesses")
            .or_insert_with(|| Value::Array(vec![]));
        obj.entry("customRules")
            .or_insert_with(|| Value::Array(vec![]));
        obj.entry("servers").or_insert_with(|| Value::Array(vec![]));
        obj.entry("subscriptions")
            .or_insert_with(|| Value::Array(vec![]));
    }
    // 语义校验（枚举/范围/必填 tunConfig）
    validate_config(value)
}

/// 默认配置（createDefaultConfig 投影）。
///
/// Polaris 锚点 `createDefaultConfig`：新装/回落用。字段与 Polaris 对齐（关键开关默认值）。
pub fn default_config() -> Value {
    serde_json::json!({
        "subscriptions": [],
        "servers": [],
        "selectedServerId": null,
        // 分流策略默认「智能」（D1，2026-08-19 陈先生拍板：rule-based 是大多数用户想要的
        // 出厂态；global 会把全部流量压进单一出口，新装即全局易被读作异常）。validate 三值
        // 均合法（global/smart/direct），此处只动出厂默认。
        "proxyMode": "smart",
        "proxyModeType": "systemProxy",
        // mtu **刻意缺席** = 自动（按最终栈 × 平台派生，见 config-engine `tun_stack::default_mtu_for`）。
        // 新装写一个具体数会把「当时的默认」冻在磁盘上，此后默认值再变也追不上——那正是存量
        // 1350/1400 需要 `migrate_tun_mtu` 清一遍的成因。
        "tunConfig": {
            "stack": "auto",
            "autoRoute": true,
            "strictRoute": true
        },
        "tunStackMigrated": true,
        "tunMtuMigrated": true,
        "customRules": [],
        "autoStart": false,
        "silentStart": false,
        "autoConnect": false,
        "minimizeToTray": true,
        "autoCheckUpdate": true,
        "autoLightweightMode": false,
        "keepTrayMenuWarm": true,
        "keepTrayMenuWarmDefaultMigrated": true,
        "hardwareAcceleration": true,
        "windowEffects": true,
        "desktopNotifications": true,
        "autoUpdateSubscriptionOnStart": true,
        "subscriptionUpdateIntervalHours": 12,
        "subscriptionProxyPolicy": "follow",
        "mainSessionViaProxy": true,
        "rememberWindowSize": true,
        "interruptConnectionsOnSwitch": true,
        "enableIPv6": false,
        "autoPrivacyMode": false,
        "privacyPassword": "",
        "dnsConfig": {
            "domesticDns": "https://doh.pub/dns-query",
            "foreignDns": "https://dns.google/dns-query",
            "enableFakeIp": true,
            "fakeIpToggleMigrated": true,
            "fakeIpTunAutoEnable": false,
            "takeoverSystemDns": true,
            "nodeResolverPool": ["ali", "dnspod"],
            "nodeResolverSingle": "ali",
            "nodeResolverMigrated": true
        },
        "customRuleSets": [],
        "appRulesSeeded": true,
        "appRoutingEnabled": false,
        "coreUpdateChannel": "stable",
        "ruleResourceAutoUpdate": true,
        "ruleResourceUpdateIntervalHours": 12,
        "fakeIpFilter": true,
        "blockQuic": true,
        "singboxDashboard": true,
        "mixedPort": 7890,
        "controlPort": 9090,
        "logLevel": "info",
        "disableLogFile": false,
        "uiTheme": "system",
        "language": "auto"
    })
}
