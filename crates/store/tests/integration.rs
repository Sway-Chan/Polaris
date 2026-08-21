//! 集成测试：ConfigStore 经 StdFs（真实 std::fs）端到端，tempfile 隔离不触碰宿主 FS。
//!
//! 重点：维度7 #7（坏 JSON 容错 + 绝不覆盖磁盘）+ 维度7 #54（迁移幂等）在真实 FS 上复验。

#![forbid(unsafe_code)]

use polaris_store::{ConfigStore, LoadResult, StdFs, StoreError};
use std::path::PathBuf;
use tempfile::TempDir;

fn cfg_path(dir: &TempDir) -> PathBuf {
    dir.path().join("config.json")
}

#[test]
fn missing_file_loads_default_and_marks_was_missing() {
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let res: LoadResult = ConfigStore::load(&StdFs, &path);
    assert!(res.was_missing);
    assert!(!res.loaded_from_disk);
    assert!(res.error.is_none());
    assert_eq!(res.config["proxyMode"], serde_json::json!("smart"));
    assert_eq!(res.config["keepTrayMenuWarm"], serde_json::json!(true));
    assert_eq!(
        res.config["keepTrayMenuWarmDefaultMigrated"],
        serde_json::json!(true)
    );
}

#[test]
fn save_then_load_round_trips() {
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let cfg = serde_json::json!({
        "proxyMode": "smart",
        "proxyModeType": "tun",
        "logLevel": "info",
        "mixedPort": 7890,
        "servers": [{"id":"s1","name":"HK","protocol":"trojan","address":"1.2.3.4","port":443,"password":"pw"}],
        "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
    });
    ConfigStore::save(&StdFs, &path, &cfg, "aabbccddeeff").unwrap();
    assert!(path.exists());
    let res = ConfigStore::load(&StdFs, &path);
    assert!(res.loaded_from_disk);
    assert_eq!(res.config["servers"].as_array().unwrap().len(), 1);
    assert_eq!(res.config["proxyModeType"], serde_json::json!("tun"));
}

#[test]
fn corrupt_json_returns_default_without_overwriting_disk() {
    // 维度7 #7 核心：坏 JSON → 内存默认，磁盘原文件原样保留。
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let corrupt = "{ not valid json at all";
    std::fs::write(&path, corrupt).unwrap();

    let res = ConfigStore::load(&StdFs, &path);
    assert!(!res.loaded_from_disk);
    assert!(!res.was_missing);
    assert!(matches!(res.error, Some(StoreError::Parse(_))));

    // 磁盘真实文件原样保留（绝不被默认配置覆盖）
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, corrupt);
}

#[test]
fn bad_field_sanitized_good_field_kept_on_disk_preserved() {
    // 维度7 #7：坏字段（servers 非 array）跳过，好字段（customRules）保留；
    // load 不落盘 → 磁盘原文件原样保留（迁移落盘由调用方决定）。
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let original = r#"{
        "proxyMode": "smart",
        "proxyModeType": "systemProxy",
        "logLevel": "info",
        "mixedPort": 7890,
        "servers": "not-an-array",
        "customRules": [{"id":"r1","type":"domain","values":["a.com"],"action":"proxy","enabled":true}],
        "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
    }"#;
    std::fs::write(&path, original).unwrap();

    let res = ConfigStore::load(&StdFs, &path);
    assert!(res.loaded_from_disk);
    // 坏字段剔除（servers → finalize 填 []）
    assert_eq!(res.config["servers"], serde_json::json!([]));
    // 好字段保留
    assert_eq!(res.config["customRules"].as_array().unwrap().len(), 1);

    // 磁盘原文件保留（load 纯逻辑不落盘）
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("\"not-an-array\""));
}

#[test]
fn migration_chain_runs_and_is_idempotent_on_real_fs() {
    // 维度7 #54：旧格式 → 迁移 → 新格式；落盘后二次 load 幂等。
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let old = r#"{
        "proxyMode": "smart",
        "proxyModeType": "TUN",
        "logLevel": "info",
        "tunConfig": {"mtu":1350,"stack":"system","autoRoute":true,"strictRoute":true},
        "keepTrayMenuWarm": false,
        "subscriptionUpdateViaProxy": true
    }"#;
    std::fs::write(&path, old).unwrap();

    let res1 = ConfigStore::load(&StdFs, &path);
    assert!(res1.loaded_from_disk);
    assert!(res1.migration_delta.changed, "首次加载有迁移变更");
    assert_eq!(res1.config["tunStackMigrated"], serde_json::json!(true));
    assert_eq!(res1.config["keepTrayMenuWarm"], serde_json::json!(true));
    assert_eq!(
        res1.config["keepTrayMenuWarmDefaultMigrated"],
        serde_json::json!(true)
    );
    assert_eq!(
        res1.config["subscriptionProxyPolicy"],
        serde_json::json!("proxy")
    );

    // 模拟运行时层把迁移后配置落盘
    ConfigStore::save(&StdFs, &path, &res1.config, "112233445566").unwrap();

    // 二次 load：已迁移，幂等（无变更）
    let res2 = ConfigStore::load(&StdFs, &path);
    assert!(res2.loaded_from_disk);
    assert!(!res2.migration_delta.changed, "已迁移配置二次加载无变更");
    assert_eq!(res2.config, res1.config, "二次加载内容稳定");

    // 迁移后用户独立关闭预热：再次保存/加载必须保留 false，不得被启动迁移反复顶回。
    let mut user_disabled = res2.config;
    user_disabled["keepTrayMenuWarm"] = serde_json::json!(false);
    ConfigStore::save(&StdFs, &path, &user_disabled, "223344556677").unwrap();
    let res3 = ConfigStore::load(&StdFs, &path);
    assert!(!res3.migration_delta.changed);
    assert_eq!(res3.config["keepTrayMenuWarm"], serde_json::json!(false));
}

#[test]
fn atomic_write_leaves_no_partial_on_success() {
    // 原子写成功后：最终文件完整，无 tmp 残留。
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let cfg = serde_json::json!({
        "proxyMode": "direct",
        "proxyModeType": "manual",
        "logLevel": "info",
        "mixedPort": 7890,
        "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
    });
    ConfigStore::save(&StdFs, &path, &cfg, "deadbeefdead").unwrap();
    // 最终文件存在且完整
    assert!(path.exists());
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("\"proxyModeType\": \"manual\""));
    // 无 tmp 残留（rename 已替换）
    let tmp = path.with_file_name("config.json.deadbeefdead.tmp");
    assert!(!tmp.exists(), "tmp 文件不应残留");
}

#[test]
fn save_validates_before_writing() {
    // save 前复跑 validate：proxyMode 非法 → Err，不创建文件。
    let dir = TempDir::new().unwrap();
    let path = cfg_path(&dir);
    let bad = serde_json::json!({
        "proxyMode": "not-a-mode",
        "proxyModeType": "tun",
        "logLevel": "info",
        "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
    });
    let res = ConfigStore::save(&StdFs, &path, &bad, "deadbeefdead");
    assert!(matches!(res, Err(StoreError::Validation(_))));
    assert!(!path.exists());
}
