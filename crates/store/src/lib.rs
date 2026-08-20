//! polaris-store — 配置持久化层纯逻辑核心。
//!
//! Polaris 锚点：`main/services/ConfigManager.ts`（loadConfig / saveConfig / sanitize / migrate /
//! validate / atomic-write 的纯逻辑核心，FS 操作经 [`fs::ConfigFs`] trait 抽象）。
//!
//! 职责：
//! - [`backup`]：选择性备份 / 恢复的类别模型（6 类分桶 + 整类替换 + 空跳过）+ 备份文件格式解析。
//! - [`sanitize`]：宽容反序列化（坏字段跳过不崩，维度7 #7 HIGH）+ 默认值填充。
//! - [`migrate`]：一次性迁移链（全量移植，幂等 + 绝不抛，维度7 #54）。
//! - [`validate`]：规则 / 节点 / 端口合法性校验 + 纯校验辅助函数（协议白名单、CIDR、规则类型）。
//! - [`fs`]：[`fs::ConfigFs`] trait + [`fs::atomic_write_plan`]（tmp→rename 原子写逻辑）。
//! - [`store`]：[`store::ConfigStore`] 编排 load / save / backup 的纯逻辑核心。
//!
//! 纯逻辑纪律：FS 收口到 trait，运行时层注入 std::fs 实现、测试注入 [`MockFs`]。
//! 不持有 currentConfig 缓存（运行时层包装维护，对齐 Polaris ConfigManager.currentConfig）。
//!
//! 见 ~/docs/polaris/design/polaris-system-design.md §B.2（crate 边界 + 职责 + Polaris 锚点）。
//! 行为不变式见 vault 的能力入口点清单（special-logic 分册）（维度7 淬炼 70 条）。

// `store::default_config()` 的 `json!` 字面量已接近默认 128 层展开上限（每个键约一层），
// 加一个键就会 `recursion limit reached`。抬到 256 是编译器给的处置，且只作用于宏展开深度，
// 不影响运行期。**别改成「把默认值拆成多个 json! 再 merge」**：那会让「默认配置长什么样」
// 从一处字面量散成几处拼装，是拿可读性换一个编译器参数。
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

pub mod backup;
pub mod error;
pub mod fs;
pub mod migrate;
pub mod privacy_lock;
pub mod sanitize;
pub mod store;
pub mod validate;

pub use backup::{
    build_backup_info, count_category, detect_categories, merge_categories, parse_backup_content,
    pick_categories, sanitize_cross_platform_rules, BackupCategory, BackupInfo, MergeOutcome,
    ParsedBackup, BACKUP_CATEGORIES, BACKUP_FILE_VERSION,
};
pub use error::StoreError;
pub use fs::{atomic_write_plan, AtomicWritePlan, ConfigFs, StdFs};
pub use migrate::{migrate_all, MigrationDelta};
pub use privacy_lock::{PrivacyPasswordHash, ScryptParams};
pub use sanitize::sanitize_config;
pub use store::{
    corrupt_backup_stamp, default_config, finalize_config, prune_corrupt_backups, ConfigStore,
    LoadResult,
};
pub use validate::validate_config;

/// 字符串去重保序（保序 = 按首次出现顺序）。复用 config-engine collections::dedupe。
///
/// 供 sanitize / migrate 内 CIDR / bypassProcesses 去重。返回新 Vec。
pub fn dedupe_str(items: &[String]) -> Vec<String> {
    polaris_config_engine::user_config::collections::dedupe(items.iter().cloned())
}

// ════════════════════════════════════════════════════════════════════════════
// 测试基础设施：内存 MockFs（实现 ConfigFs，不触碰宿主 FS）+ FsOp 记录。
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
pub use test_support::{FsOp, MockFs};

#[cfg(test)]
mod test_support {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use crate::fs::ConfigFs;
    use crate::StoreError;

    /// 记录的 FS 操作（用于断言 atomic_write 的 tmp→rename 顺序）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum FsOp {
        Write(PathBuf, String),
        Rename(PathBuf, PathBuf),
        Mkdir(PathBuf),
        Copy(PathBuf, PathBuf),
        Remove(PathBuf),
    }

    #[derive(Debug, Default)]
    struct MockFsInner {
        files: HashMap<PathBuf, String>,
        ops: Vec<FsOp>,
    }

    /// 内存文件系统 mock（实现 ConfigFs，不触碰宿主 FS）。
    /// 内部用 RefCell 实现 &self 下的可变性（对齐 ConfigFs 的 &self 签名，运行时 std::fs 亦 &self）。
    #[derive(Debug, Default)]
    pub struct MockFs {
        inner: RefCell<MockFsInner>,
    }

    impl MockFs {
        /// 预置一个文件（用于测试 setup）。
        pub fn with(self, path: &Path, content: impl Into<String>) -> Self {
            self.inner
                .borrow_mut()
                .files
                .insert(path.to_path_buf(), content.into());
            self
        }

        /// 取所有记录的操作（断言用）。
        pub fn operations(&self) -> Vec<FsOp> {
            self.inner.borrow().ops.clone()
        }

        /// 取文件内容快照（断言用）。
        pub fn snapshot(&self, path: &Path) -> Option<String> {
            self.inner.borrow().files.get(path).cloned()
        }
    }

    impl ConfigFs for MockFs {
        fn read_to_string(&self, path: &Path) -> Result<String, StoreError> {
            self.inner
                .borrow()
                .files
                .get(path)
                .cloned()
                .ok_or_else(|| StoreError::Io(format!("not found: {}", path.display())))
        }

        fn write(&self, path: &Path, content: &str) -> Result<(), StoreError> {
            let mut inner = self.inner.borrow_mut();
            inner
                .ops
                .push(FsOp::Write(path.to_path_buf(), content.to_string()));
            inner.files.insert(path.to_path_buf(), content.to_string());
            Ok(())
        }

        fn rename(&self, from: &Path, to: &Path) -> Result<(), StoreError> {
            let mut inner = self.inner.borrow_mut();
            inner
                .ops
                .push(FsOp::Rename(from.to_path_buf(), to.to_path_buf()));
            let content = inner.files.remove(from);
            if let Some(c) = content {
                inner.files.insert(to.to_path_buf(), c);
                Ok(())
            } else {
                Err(StoreError::Io(format!("not found: {}", from.display())))
            }
        }

        fn create_dir_all(&self, path: &Path) -> Result<(), StoreError> {
            self.inner
                .borrow_mut()
                .ops
                .push(FsOp::Mkdir(path.to_path_buf()));
            Ok(())
        }

        fn copy(&self, from: &Path, to: &Path) -> Result<(), StoreError> {
            let mut inner = self.inner.borrow_mut();
            inner
                .ops
                .push(FsOp::Copy(from.to_path_buf(), to.to_path_buf()));
            let content = inner.files.get(from).cloned();
            if let Some(c) = content {
                inner.files.insert(to.to_path_buf(), c);
                Ok(())
            } else {
                Err(StoreError::Io(format!("not found: {}", from.display())))
            }
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.borrow().files.contains_key(path)
        }

        fn remove(&self, path: &Path) -> Result<(), StoreError> {
            let mut inner = self.inner.borrow_mut();
            inner.ops.push(FsOp::Remove(path.to_path_buf()));
            inner.files.remove(path);
            Ok(())
        }

        fn list_dir(&self, dir: &Path) -> Result<Vec<String>, StoreError> {
            // 返回 parent==dir 的文件 basename（对齐 std::fs::read_dir 的直接子项语义）。
            let inner = self.inner.borrow();
            let names: Vec<String> = inner
                .files
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
                .collect();
            Ok(names)
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// ConfigStore 端到端测试（内存 MockFs，不触碰宿主 FS）。
// 重点覆盖维度7 #7（坏 JSON 容错 + 绝不覆盖磁盘真实文件）与 save→load 往返。
// ════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod store_tests {
    use super::*;
    use std::path::Path;

    const CFG: &str = "/data/config.json";

    #[test]
    fn load_missing_file_returns_default_and_marks_was_missing() {
        // 新装：文件不存在 → 返回默认配置 + was_missing=true（调用方可安全落盘默认）。
        let fs = MockFs::default();
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(!res.loaded_from_disk);
        assert!(res.was_missing);
        assert!(res.error.is_none());
        // 默认配置有 expected 字段
        assert_eq!(res.config["proxyMode"], serde_json::json!("smart"));
        assert_eq!(
            res.config["proxyModeType"],
            serde_json::json!("systemProxy")
        );
        assert_eq!(res.config["mixedPort"], serde_json::json!(7890));
    }

    #[test]
    fn load_valid_config_round_trips_through_save() {
        // save → load 往返：落盘的配置能原样加载回来（关键字段不丢）。
        let fs = MockFs::default();
        let cfg = serde_json::json!({
            "proxyMode": "smart",
            "proxyModeType": "tun",
            "logLevel": "warn",
            "mixedPort": 8080,
            "controlPort": 9091,
            "servers": [{"id":"s1","name":"HK","protocol":"trojan","address":"1.2.3.4","port":443,"password":"pw"}],
            "customRules": [{"id":"r1","type":"domain","values":["a.com"],"action":"direct","enabled":true}],
            "tunConfig": {"mtu":1400,"stack":"gvisor","autoRoute":true,"strictRoute":false}
        });
        ConfigStore::save(&fs, Path::new(CFG), &cfg, "abcdef012345").unwrap();
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(res.loaded_from_disk);
        assert!(res.error.is_none());
        assert_eq!(res.config["proxyMode"], serde_json::json!("smart"));
        assert_eq!(res.config["proxyModeType"], serde_json::json!("tun"));
        assert_eq!(res.config["mixedPort"], serde_json::json!(8080));
        assert_eq!(res.config["servers"].as_array().unwrap().len(), 1);
        assert_eq!(res.config["customRules"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn load_corrupt_json_returns_default_and_does_not_overwrite_disk() {
        // 维度7 #7 核心：坏 JSON → 内存回落默认，**磁盘真实文件原样保留**（绝不覆盖）。
        let corrupt = "{ this is not valid json";
        let fs = MockFs::default().with(Path::new(CFG), corrupt);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(!res.loaded_from_disk);
        assert!(!res.was_missing);
        assert!(matches!(res.error, Some(StoreError::Parse(_))));
        // 磁盘真实文件原样保留（未被默认配置覆盖）
        assert_eq!(fs.snapshot(Path::new(CFG)).as_deref(), Some(corrupt));
    }

    #[test]
    fn load_bad_field_keeps_good_fields_and_preserves_disk() {
        // 维度7 #7：坏字段（servers 非 array）被跳过，好字段（customRules）保留；
        // 磁盘原文件原样保留（sanitize 只改内存 Value，不落盘）。
        let original = r#"{
            "proxyMode": "smart",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 7890,
            "servers": "not-an-array",
            "customRules": [{"id":"r1","type":"domain","values":["a.com"],"action":"proxy","enabled":true}],
            "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), original);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(res.loaded_from_disk);
        assert!(res.error.is_none());
        // 坏字段已从内存配置剔除（servers 缺失 → finalize 填 []）
        assert_eq!(res.config["servers"], serde_json::json!([]));
        // 好字段保留
        assert_eq!(res.config["customRules"].as_array().unwrap().len(), 1);
        // 磁盘原文件原样保留（load 不落盘；迁移落盘由调用方 best-effort 决定）
        assert_eq!(fs.snapshot(Path::new(CFG)).as_deref(), Some(original));
    }

    #[test]
    fn load_bad_server_dropped_good_server_kept() {
        // 维度7 #7：逐节点 sanitize——坏节点剔除、好节点保留。
        let json = r#"{
            "proxyMode": "smart",
            "proxyModeType": "tun",
            "logLevel": "info",
            "mixedPort": 7890,
            "servers": [
                {"id":"good","name":"Good","protocol":"trojan","address":"1.2.3.4","port":443,"password":"pw"},
                {"id":"","name":"Bad","protocol":"trojan"},
                {"id":"unknown","name":"U","protocol":"nonexistent","address":"5.6.7.8","port":80}
            ],
            "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), json);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(res.loaded_from_disk);
        let servers = res.config["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["id"], serde_json::json!("good"));
    }

    #[test]
    fn save_rejects_invalid_config() {
        // save 前复跑 validate：proxyMode 非法 → Err，不落盘。
        let fs = MockFs::default();
        let bad = serde_json::json!({
            "proxyMode": "invalid-mode",
            "proxyModeType": "tun",
            "logLevel": "info",
            "tunConfig": {"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true}
        });
        let res = ConfigStore::save(&fs, Path::new(CFG), &bad, "abcdef012345");
        assert!(matches!(res, Err(StoreError::Validation(_))));
        assert!(!fs.exists(Path::new(CFG)));
    }

    #[test]
    fn save_uses_atomic_tmp_then_rename() {
        // 原子写：经 tmp 文件 → rename，不直接覆盖（防半写截断）。
        let fs = MockFs::default();
        let cfg = default_config();
        ConfigStore::save(&fs, Path::new(CFG), &cfg, "abcdef012345").unwrap();
        let ops = fs.operations();
        // 应有 Write(tmp) + Rename(tmp→final)
        assert!(ops.iter().any(|o| matches!(
            o,
            FsOp::Write(p, _) if p.to_string_lossy().ends_with("abcdef012345.tmp")
        )));
        assert!(ops.iter().any(|o| matches!(
            o,
            FsOp::Rename(from, _) if from.to_string_lossy().ends_with("abcdef012345.tmp")
        )));
        // 最终文件存在
        assert!(fs.exists(Path::new(CFG)));
    }

    #[test]
    fn load_migrates_old_format_and_is_idempotent() {
        // 维度7 #54：旧格式 → 迁移 → 新格式；二次 load（已迁移）幂等。
        let old = r#"{
            "proxyMode": "smart",
            "proxyModeType": "TUN",
            "logLevel": "info",
            "tunConfig": {"mtu":1350,"stack":"system","autoRoute":true,"strictRoute":true},
            "subscriptionUpdateViaProxy": true
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), old);
        let res1 = ConfigStore::load(&fs, Path::new(CFG));
        assert!(res1.loaded_from_disk);
        assert!(res1.migration_delta.changed, "首次加载有迁移变更");
        // 迁移效果
        assert_eq!(res1.config["tunStackMigrated"], serde_json::json!(true));
        assert_eq!(
            res1.config["subscriptionProxyPolicy"],
            serde_json::json!("proxy")
        );
        assert!(!res1
            .config
            .as_object()
            .unwrap()
            .contains_key("subscriptionUpdateViaProxy"));
        assert_eq!(
            res1.config["dnsConfig"]["fakeIpToggleMigrated"],
            serde_json::json!(true)
        );

        // 模拟落盘迁移后配置 → 二次 load 幂等（migration_delta.changed=false）
        let migrated = res1.config.clone();
        let fs2 = MockFs::default().with(Path::new(CFG), migrated.to_string());
        let res2 = ConfigStore::load(&fs2, Path::new(CFG));
        assert!(res2.loaded_from_disk);
        assert!(!res2.migration_delta.changed, "已迁移配置二次加载无变更");
        // 内容稳定（幂等）
        assert_eq!(res2.config, migrated);
    }

    #[test]
    fn load_validation_failure_returns_default_preserves_disk() {
        // validate 失败（缺 tunConfig）→ 回落默认，磁盘原文件保留。
        let bad = r#"{
            "proxyMode": "smart",
            "proxyModeType": "tun",
            "logLevel": "info",
            "mixedPort": 7890
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), bad);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(!res.loaded_from_disk);
        assert!(matches!(res.error, Some(StoreError::Validation(_))));
        // 磁盘原文件保留
        assert_eq!(fs.snapshot(Path::new(CFG)).as_deref(), Some(bad));
        // 内存回落默认配置
        assert_eq!(res.config["proxyMode"], serde_json::json!("smart"));
    }

    #[test]
    fn load_corrupt_wires_backup_corrupt() {
        // 该修（HIGH）：损坏配置 load 失败分支须调 backup_corrupt（此前 load 只回落默认、从不备份）。
        let corrupt = "{ this is not valid json";
        let fs = MockFs::default().with(Path::new(CFG), corrupt);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(!res.loaded_from_disk);
        // 原文件原样保留（copy 不覆盖）
        assert_eq!(fs.snapshot(Path::new(CFG)).as_deref(), Some(corrupt));
        // 生成了一份 config.corrupt-<ts>.json，内容 == 损坏原文件
        let dir = Path::new("/data");
        let backups: Vec<String> = fs
            .list_dir(dir)
            .unwrap()
            .into_iter()
            .filter(|n| n.starts_with("config.corrupt-") && n.ends_with(".json"))
            .collect();
        assert_eq!(backups.len(), 1, "损坏配置须备份一份快照");
        assert_eq!(
            fs.snapshot(&dir.join(&backups[0])).as_deref(),
            Some(corrupt),
            "备份内容须 == 损坏原文件（保留人工恢复机会）"
        );
    }

    #[test]
    fn load_validation_failure_wires_backup_corrupt() {
        // validate 失败分支（缺 tunConfig）同样须备份（非仅 JSON 坏）。
        let bad =
            r#"{"proxyMode":"smart","proxyModeType":"tun","logLevel":"info","mixedPort":7890}"#;
        let fs = MockFs::default().with(Path::new(CFG), bad);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(!res.loaded_from_disk);
        let dir = Path::new("/data");
        let n = fs
            .list_dir(dir)
            .unwrap()
            .into_iter()
            .filter(|n| n.starts_with("config.corrupt-") && n.ends_with(".json"))
            .count();
        assert_eq!(n, 1, "校验失败也须备份");
    }

    #[test]
    fn load_corrupt_prunes_backups_to_two() {
        // 保留最近 2 份：预置 3 份旧备份 + 本次损坏再产 1 份 → prune 后仅剩 2。
        let corrupt = "{ broken";
        let dir = Path::new("/data");
        let mut fs = MockFs::default().with(Path::new(CFG), corrupt);
        for i in 1..=3u64 {
            fs = fs.with(
                &dir.join(format!("config.corrupt-{i:014}-000000000.json")),
                "old",
            );
        }
        let _ = ConfigStore::load(&fs, Path::new(CFG));
        let backups: Vec<String> = fs
            .list_dir(dir)
            .unwrap()
            .into_iter()
            .filter(|n| n.starts_with("config.corrupt-") && n.ends_with(".json"))
            .collect();
        assert_eq!(backups.len(), 2, "prune 后仅保留最近 2 份损坏备份");
    }

    #[test]
    fn load_backs_up_before_rule_migration() {
        // 该修（MED）：含旧 DomainRule 的配置 load 时须先落 .pre-rule-migration.bak（判据看原始 JSON，
        // 因 sanitize 会丢弃无 type 的旧规则；备份保原始供回滚）。
        let legacy = r#"{
            "proxyMode":"smart","proxyModeType":"systemProxy","logLevel":"info","mixedPort":7890,
            "tunConfig":{"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true},
            "customRules":[{"id":"a","domains":["x.com"],"action":"proxy","enabled":true}]
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), legacy);
        let _ = ConfigStore::load(&fs, Path::new(CFG));
        let bak = Path::new("/data/config.json.pre-rule-migration.bak");
        assert_eq!(
            fs.snapshot(bak).as_deref(),
            Some(legacy),
            "迁移前须备份原始配置（含旧规则）"
        );
    }

    #[test]
    fn load_preserves_legacy_domain_rule_through_migration() {
        // 项1 修（HIGH · sanitize-strips-legacy-rules-before-migrate）：含 1 条旧 DomainRule（无 type + domains）
        // 的配置过 load 后规则须**存活**（迁移为 domainSuffix Rule），而非被 sanitize 在 migrate 之前剥光 →
        // customRules==[] 静默全丢。变异牙：打断 `sanitize_custom_rules` 的旧规则放行（回归到「无 type 即丢弃」）→
        // migrate 阶段无旧规则可迁 → rules.len()==0，本测转红。
        let legacy = r#"{
            "proxyMode":"smart","proxyModeType":"systemProxy","logLevel":"info","mixedPort":7890,
            "tunConfig":{"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true},
            "customRules":[{"id":"legacy-1","domains":["example.com"],"action":"proxy","enabled":true}]
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), legacy);
        let res = ConfigStore::load(&fs, Path::new(CFG));
        assert!(res.loaded_from_disk);
        let rules = res.config["customRules"].as_array().unwrap();
        assert_eq!(
            rules.len(),
            1,
            "旧规则迁移后存活（非 sanitize 先剥光致静默全丢）"
        );
        assert_eq!(
            rules[0]["type"],
            serde_json::json!("domainSuffix"),
            "旧 DomainRule 迁移为 domainSuffix Rule"
        );
        assert_eq!(rules[0]["id"], serde_json::json!("legacy-1"), "id 保留");
        assert_eq!(
            rules[0]["values"][0],
            serde_json::json!("example.com"),
            "domains → values"
        );
    }

    #[test]
    fn load_no_rule_backup_for_modern_rules() {
        // 幂等/无误备：新 shape 规则（含 type）不触发 .pre-rule-migration.bak。
        let modern = r#"{
            "proxyMode":"smart","proxyModeType":"systemProxy","logLevel":"info","mixedPort":7890,
            "tunConfig":{"mtu":1350,"stack":"auto","autoRoute":true,"strictRoute":true},
            "customRules":[{"id":"a","type":"domainSuffix","values":["x.com"],"action":"proxy","enabled":true}]
        }"#;
        let fs = MockFs::default().with(Path::new(CFG), modern);
        let _ = ConfigStore::load(&fs, Path::new(CFG));
        assert!(
            fs.snapshot(Path::new("/data/config.json.pre-rule-migration.bak"))
                .is_none(),
            "新 shape 规则不该产生迁移备份"
        );
    }

    /// 🔴 新装配置**不得**写死 MTU（2026-08-05 起）。
    ///
    /// 此前按平台写 mac 1400 / 其余 1350。那个模型有两处错：① 默认 MTU 真正的自变量是**栈**不是平台
    /// （gvisor 吃得下 65535，system/mixed 在 65535 下塌到 11 Mbps）；② 一旦落盘成具体数，此后默认值
    /// 再怎么改都追不上这台机器——存量 1350/1400 正因此需要 `migrate_tun_mtu` 清一遍。
    ///
    /// 缺席 = 自动，由 config-engine 在生成期按最终栈 × 平台派生。
    #[test]
    fn default_config_leaves_mtu_absent() {
        let cfg = default_config();
        assert!(
            cfg["tunConfig"].get("mtu").is_none(),
            "新装不得把 MTU 冻在磁盘上：{}",
            cfg["tunConfig"]
        );
        // 新装同时置迁移标记：否则首次启动会对一份本就正确的配置再跑一次迁移（无害但 changed=true，
        // 会白写一次盘 + 白发一次 configChanged）。
        assert_eq!(cfg["tunMtuMigrated"], serde_json::json!(true));
        assert_eq!(
            cfg["keepTrayMenuWarm"],
            serde_json::json!(true),
            "托盘 warm 默认开启，以约 30–40MB renderer 常驻换取无冷启滞后的托盘交互"
        );
    }

    #[test]
    fn backup_corrupt_copies_without_overwriting_original() {
        // 损坏备份：copy 到 config.corrupt-<ts>.json，原文件不动。
        let corrupt = "{ broken";
        let fs = MockFs::default().with(Path::new(CFG), corrupt);
        ConfigStore::backup_corrupt(&fs, Path::new(CFG), "2026-07-15T00-00-00Z");
        let backup = Path::new("/data/config.corrupt-2026-07-15T00-00-00Z.json");
        assert_eq!(fs.snapshot(backup).as_deref(), Some(corrupt));
        // 原文件仍在
        assert_eq!(fs.snapshot(Path::new(CFG)).as_deref(), Some(corrupt));
    }
}
