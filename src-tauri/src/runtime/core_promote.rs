//! 受保护核提升 + 起核后「实跑二进制」自证（换核在 TUN 提权路径上真正生效的两块）。
//!
//! # 为什么必须有这个模块（第一性）
//!
//! `core_paths` / `core_swap` 的「可写现役核 + 随包种子」模型解决的是**app 直起**那条腿：
//! 换核写 `<config_dir>/core_update/sing-box`，`resolve_core_binary()` 读同一路径 ⇒ 写=读=执行，
//! 免提权且自洽。
//!
//! 但 **TUN 提权路径根本不走那个文件**：mac/win 的 `start` 协议**不带核路径**，helper 恒执行自己
//! 启动时 `--singbox` 锁定的那一个（`crates/helper/src/platform/macos/handler.rs:51`「防写任意路径」/
//! `daemon.rs:44` 从 flag 取；linux 带路径但 helper 强制它 == 锁定的 `coredir/sing-box`，否则
//! `ERR core-path-denied`，`platform/linux/handler.rs:350`）。那是**安全边界**，不能为了换核去松绑
//! ——「持 token 就能让 root 跑任意二进制」比跑旧核严重得多。
//!
//! 于是唯一正确的做法与 上游 一致：**把新核推进受保护目录**，让「helper 锁定的那个路径」的**内容**变新。
//! 路径不变 ⇒ helper 无需重启即在下次 `start` 时 exec 到新核（helper 每次 `start` 现 spawn，不持句柄）。
//! 这条腿在 上游 里是 `HelperManager.installCore` → Go `installCore`（`helper/helper.go:127-198`），
//! Polaris 移植时 **helper 侧全实现了**（[`polaris_helper::core_install`] + `Request::InstallCore`），
//! **app 侧从未调用** —— 缺的就是这一环。
//!
//! # 提升的时机：每次经 helper 起核前对账，而非「换核成功后推一次」
//!
//! 上游 在换核成功后立刻 `installCore`。那样只覆盖「换核」这一个触发点，而受保护核会因
//! **至少三条**别的路径与现役核漂移：
//!  1. **app 升级重播种**（`core_paths` 的 reseed：随包基线变新 → 重写 `core_update/`）——p101 实测正是这条；
//!  2. **helper 装得比核晚 / 装完再换核**；
//!  3. 回滚 / reset-factory / 手动上传替换。
//!
//! 故本模块把它做成**起核前的幂等对账**（hash 相同即零动作），覆盖「helper 已在跑」这个常态，
//! 而不是挂在某一个变更事件上。
//!
//! # 安全模型（与 上游 同）
//!
//! 源是用户可写文件，落点是 root 目录 —— 这不是新增攻击面：helper 侧 `install-core` 只写**锁定的**
//! `coredir`（不接受任意目标路径），且**读全字节进内存做 sha256 校验后再落盘**（堵 TOCTOU，
//! `core_install.rs:15-16`）。提权面的收益在**执行时**：root exec 的是 root 拥有的文件，
//! 用户此后改不动它。

use std::path::{Path, PathBuf};

use polaris_helper_proto::Platform;

use crate::runtime::core_paths::core_filename_for;

/// 随核一并进受保护目录的**配套库前缀**（naive 出站的 cronet）。
///
/// 受保护核目录是 helper `install-core` 的**独占**领地：它落盘后会把 `src_dir` 里没有的文件
/// **全部删掉**（`core_install::prune_extra_files`，移植自 `helper.go:179-192`）。linux 安装脚本
/// 会随核播种 `libcronet.so`（`manager.rs:879-880`），若提升时只带 `sing-box`，那个 cronet 会被
/// prune 顺手删掉 ⇒ naive 出站静默失效。故 allowlist 必须带上它。
pub use crate::runtime::core_paths::CORE_SIDECAR_PREFIX;

/// 暂存目录名（`<config_dir>/core-promote/`）：喂给 `install-core` 的**干净** `src_dir`。
///
/// **为什么不能直接把 `core_update/` 当 src_dir**：那个目录还住着 `.core-seed.json`（播种簿记）
/// 与 `sing-box.bak`（回滚备份，与核同样 80MB）。`install-core` 会把 src_dir 里**每一个非目录文件**
/// 都复制进受保护目录 ⇒ 簿记与备份一起被搬进 root 目录（既无意义又白占 80MB）。
pub const CORE_PROMOTE_DIR_NAME: &str = "core-promote";

// ── 纯函数：挑文件 / 决策 ────────────────────────────────────────────────────

/// 从源目录清单里挑出**该进受保护核目录**的文件名（**纯函数**，字典序去重后返回）。
///
/// allowlist 而非 denylist：核目录内容由 helper 侧 prune 独占对齐，宁可漏带一个未知配套
/// （表现为该配套失效，可查），也不能把 `.bak` / 簿记 / 临时文件搬进 root 目录。
#[must_use]
pub fn promote_names(entries: &[String], core_filename: &str) -> Vec<String> {
    let mut names: Vec<String> = entries
        .iter()
        .filter(|n| n.as_str() == core_filename || n.starts_with(CORE_SIDECAR_PREFIX))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

/// 提升决策（**纯函数**）：源与受保护核的 sha256 对账。
///
/// `dest_hash = None` = 受保护核不存在/读不到 ⇒ 必须提升（首装后被 `[ ! -x ]` 跳过播种的机器亦然）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteDecision {
    /// 受保护核已与现役核逐字节相同 → 零动作（稳态每次起核走这里）。
    UpToDate,
    /// 需要经 helper `install-core` 推一次。
    Promote,
}

/// 决策：内容相同即跳过（**大小写不敏感比 hex**，对齐 helper 侧 `EqualFold` 口径）。
#[must_use]
pub fn decide_promote(src_hash: &str, dest_hash: Option<&str>) -> PromoteDecision {
    match dest_hash {
        Some(d) if d.eq_ignore_ascii_case(src_hash) && !src_hash.is_empty() => {
            PromoteDecision::UpToDate
        }
        _ => PromoteDecision::Promote,
    }
}

/// 本平台是否有「受保护核目录」这个概念（= `install-core` 是否可用）。
///
/// Windows **无** `install-core`：其核走 app 侧，helper 的 `--singbox` 在安装期就指向 app 侧核路径
/// （`runtime/helper.rs::install_params` 传 `resolve_core_binary()`），路径不变而内容随换核更新 ⇒
/// 无需提升（`command.rs:58` 已记此差异）。
#[must_use]
pub const fn platform_has_protected_core(platform: Platform) -> bool {
    !matches!(platform, Platform::Win)
}

// ── 起核后自证：对账「实跑二进制」而非「两份同源配置」────────────────────────

/// 「实跑二进制 == 本次期望核」的自证结论。
///
/// # 与 `proxy::attest_effective_exit`（出口自证）的**关键区别**
///
/// 那一条是**纯静态对账**（自述「纯函数、零 I/O」「不用探针」）：拿本次生成的 config 与落盘的用户
/// 意图互校 —— 两个输入同源于「意图」，故**意图正确而事实偏离**时它一律判通过。本条不重蹈：
/// 唯一的输入 `running` 来自**内核对该 pid 的记账**（linux `/proc/<pid>/exe`、mac `ps -o comm=`），
/// 版本来自**对那个文件真跑一次 `version`**。两者都是事实，不是意图的副本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreBinaryAttestation {
    /// 实跑 exe 就是本次解析出的那个文件 → 版本无需再问（app 直起腿的稳态）。
    SamePath,
    /// 路径不同但版本一致 → 受保护核已是现役核的副本（TUN 提权腿修好后的稳态）。
    SameVersion {
        /// 实跑二进制路径。
        running: PathBuf,
        /// 双方一致的版本行。
        version: String,
    },
    /// **版本不一致** —— 今天这个缺陷的正面命中（跑 alpha.45 而期望 beta.3）。
    VersionMismatch {
        /// 实跑二进制路径。
        running: PathBuf,
        /// 实跑二进制自报版本行。
        running_version: String,
        /// 本次期望的核版本行。
        expected_version: String,
    },
    /// 路径不同，且至少一侧版本读不出来 ⇒ **无法确认跑的是期望的核**。
    ///
    /// 判**告警**而非放行：既然实跑的是一个我们没直接挑的文件，"读不出它是什么" 与 "读出来不对"
    /// 对用户是同一件事——都不能宣称换核已生效。
    VersionUnreadable {
        /// 实跑二进制路径。
        running: PathBuf,
        /// 实跑二进制自报版本行（可能为空）。
        running_version: String,
        /// 本次期望的核版本行（可能为空）。
        expected_version: String,
    },
    /// 拿不到实跑 exe（内核记账读不到 / 平台不支持）⇒ 无从判定。
    ///
    /// **不报通过、也不报错**：没有观测到不等于观测到没有问题。只落 warn 日志。
    Unobservable,
}

impl CoreBinaryAttestation {
    /// 是否构成「必须让用户看见」的降级（→ `set_nonfatal_error`）。
    #[must_use]
    pub const fn is_alarm(&self) -> bool {
        matches!(
            self,
            Self::VersionMismatch { .. } | Self::VersionUnreadable { .. }
        )
    }

    /// 用户可见文案（中文；路径与版本原样给出，便于用户/日志自查）。
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::SamePath => "内核自证通过：实跑二进制 == 本次解析的核".to_owned(),
            Self::SameVersion { running, version } => format!(
                "内核自证通过：实跑 {} 与本次期望同版本（{version}）",
                running.display()
            ),
            Self::VersionMismatch {
                running,
                running_version,
                expected_version,
            } => format!(
                "内核版本不一致：实际运行的是 {}（{running_version}），\
                 而本次期望的是 {expected_version}。换核未对提权内核生效——\
                 请重装提权助手，或在设置里重新执行一次内核更新。",
                running.display()
            ),
            Self::VersionUnreadable {
                running,
                running_version,
                expected_version,
            } => format!(
                "内核版本无法确认：实际运行的是 {}（版本读数「{}」），本次期望「{}」。\
                 无法确认换核已生效。",
                running.display(),
                if running_version.is_empty() {
                    "读不到"
                } else {
                    running_version
                },
                if expected_version.is_empty() {
                    "读不到"
                } else {
                    expected_version
                },
            ),
            // 措辞刻意不含「通过」二字：本条是「没观测到」，而 `attest_unobservable_is_neither_pass_nor_alarm`
            // 正以子串断言钉死它 —— 日志里出现「…通过」会让人在事后排查时把未判定读成已判定。
            Self::Unobservable => {
                "内核自证未能进行：读不到该进程的可执行文件路径（无法判定，不作结论）".to_owned()
            }
        }
    }
}

/// 自证判定（**纯函数**；观测腿在 `proxy.rs`，此处只吃已观测到的事实）。
///
/// - `expected`：本次 `core_binary_for_start()` 解析出的核路径（app 的意图）。
/// - `running`：**内核记账的**该 pid 实跑 exe（`None` = 观测失败）。
/// - `expected_version` / `running_version`：对两个**文件**各跑一次 `version` 读到的原始首行；
///   读失败恒空串（**绝不回落随包基线**——那会把「读不到」伪装成「就是基线」，
///   与 `updater::read_core_version_line` 同一纪律）。
#[must_use]
pub fn attest_core_binary(
    expected: &Path,
    running: Option<&Path>,
    expected_version: &str,
    running_version: &str,
) -> CoreBinaryAttestation {
    let Some(running) = running else {
        return CoreBinaryAttestation::Unobservable;
    };
    if same_file_path(expected, running) {
        return CoreBinaryAttestation::SamePath;
    }
    let (rv, ev) = (running_version.trim(), expected_version.trim());
    if rv.is_empty() || ev.is_empty() {
        return CoreBinaryAttestation::VersionUnreadable {
            running: running.to_path_buf(),
            running_version: rv.to_owned(),
            expected_version: ev.to_owned(),
        };
    }
    if rv == ev {
        return CoreBinaryAttestation::SameVersion {
            running: running.to_path_buf(),
            version: rv.to_owned(),
        };
    }
    CoreBinaryAttestation::VersionMismatch {
        running: running.to_path_buf(),
        running_version: rv.to_owned(),
        expected_version: ev.to_owned(),
    }
}

/// 两个路径是否指向同一文件（**纯路径判定 + 尽力 canonicalize**）。
///
/// 先比字面（覆盖单测与绝大多数生产情形），再比 canonicalize 后的形（覆盖 symlink /
/// `/var`→`/private/var` 这类 macOS 特有的等价改写）。canonicalize 失败即退回字面结论，
/// **不因 I/O 失败就误判成"不同"**（那会平白造出一条假告警）。
fn same_file_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// ── 薄执行腿（FS / 子进程）────────────────────────────────────────────────────

/// 整文件 sha256（hex 小写）。
///
/// # Errors
///
/// 读文件失败（不存在 / 权限）。
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("读内核文件失败 {}: {e}", path.display()))?;
    Ok(polaris_updater::verify::sha256_hex(&bytes))
}

/// 把现役核 + 配套准备进一个**干净的暂存目录**，返回该目录路径。
///
/// 优先 `hard_link`（同一文件系统内零拷贝——现役核 80MB 量级，每次提升都真拷一遍是白烧 I/O），
/// 失败回落 `copy`（跨设备 / 文件系统不支持硬链）。目录**先清后建**，杜绝上一轮残留混入。
///
/// # Errors
///
/// 建目录 / 枚举源目录 / 链接与复制 全失败。
pub fn stage_promote_dir(
    src_dir: &Path,
    staged_dir: &Path,
    names: &[String],
) -> Result<(), String> {
    // 先清后建：残留文件会被 install-core 一并搬进受保护目录。
    let _ = std::fs::remove_dir_all(staged_dir);
    std::fs::create_dir_all(staged_dir)
        .map_err(|e| format!("建内核提升暂存目录失败 {}: {e}", staged_dir.display()))?;
    for name in names {
        let (from, to) = (src_dir.join(name), staged_dir.join(name));
        if std::fs::hard_link(&from, &to).is_ok() {
            continue;
        }
        std::fs::copy(&from, &to).map_err(|e| {
            format!(
                "暂存内核文件失败 {} → {}: {e}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

/// 列目录的文件名（非目录项）。读失败 → 空清单（调用方据此判「没得挑」）。
#[must_use]
pub fn list_file_names(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// 受保护核目录里的核文件路径（纯路径）。
#[must_use]
pub fn protected_core_path_in(core_dir: &Path, os: &str) -> PathBuf {
    core_dir.join(core_filename_for(os))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::TmpDir;

    // ── promote_names（allowlist）──

    #[test]
    fn promote_names_keeps_core_and_cronet_only() {
        let entries: Vec<String> = [
            "sing-box",
            "sing-box.bak",
            ".core-seed.json",
            "libcronet.so",
            "libcronet.dylib",
            "junk.tmp",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let got = promote_names(&entries, "sing-box");
        assert_eq!(
            got,
            vec![
                "libcronet.dylib".to_owned(),
                "libcronet.so".to_owned(),
                "sing-box".to_owned()
            ]
        );
    }

    /// 🟡 **门：备份与簿记绝不能进受保护目录**。
    ///
    /// 这不是洁癖：`install-core` 会把 src_dir 的每个文件都搬进 root 目录，`sing-box.bak` 与核同尺寸
    /// （实测 80MB），簿记则毫无意义。
    ///
    /// **变异探针**：把 [`promote_names`] 的 `filter` 改成恒 `true`（或删掉 `.bak` 之外的条件），本门转红。
    #[test]
    fn promote_names_excludes_backup_and_marker() {
        let entries: Vec<String> = ["sing-box", "sing-box.bak", ".core-seed.json"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let got = promote_names(&entries, "sing-box");
        assert!(
            !got.iter().any(|n| n.ends_with(".bak")),
            "备份文件绝不能进受保护核目录，实得 {got:?}"
        );
        assert!(
            !got.iter().any(|n| n.starts_with(".core-seed")),
            "播种簿记绝不能进受保护核目录，实得 {got:?}"
        );
    }

    #[test]
    fn promote_names_windows_filename() {
        let entries: Vec<String> = ["sing-box.exe", "sing-box"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            promote_names(&entries, "sing-box.exe"),
            vec!["sing-box.exe".to_owned()]
        );
    }

    // ── decide_promote ──

    #[test]
    fn decide_promote_skips_when_hash_equal() {
        let h = "a".repeat(64);
        assert_eq!(
            decide_promote(&h, Some(&h.to_uppercase())),
            PromoteDecision::UpToDate,
            "hex 比对须大小写不敏感（对齐 helper 侧 EqualFold）"
        );
    }

    #[test]
    fn decide_promote_when_dest_absent_or_differs() {
        let h = "a".repeat(64);
        assert_eq!(decide_promote(&h, None), PromoteDecision::Promote);
        assert_eq!(
            decide_promote(&h, Some(&"b".repeat(64))),
            PromoteDecision::Promote
        );
    }

    /// 空源 hash 绝不能判「已最新」（否则读不出源就静默跳过提升 = 又一条静默失效路径）。
    #[test]
    fn decide_promote_empty_src_hash_never_up_to_date() {
        assert_eq!(decide_promote("", Some("")), PromoteDecision::Promote);
    }

    #[test]
    fn windows_has_no_protected_core() {
        assert!(!platform_has_protected_core(Platform::Win));
        assert!(platform_has_protected_core(Platform::Mac));
        assert!(platform_has_protected_core(Platform::Linux));
    }

    // ── attest_core_binary ──

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn attest_same_path_passes_without_versions() {
        let a = p("/x/sing-box");
        // 版本双空也判通过：同一个文件，版本无需再问。
        assert_eq!(
            attest_core_binary(&a, Some(&a), "", ""),
            CoreBinaryAttestation::SamePath
        );
    }

    #[test]
    fn attest_same_version_across_paths_passes() {
        let got = attest_core_binary(
            &p("/u/core_update/sing-box"),
            Some(&p("/Library/Application Support/Polaris/core/sing-box")),
            "sing-box version 1.14.0-beta.3",
            "sing-box version 1.14.0-beta.3",
        );
        assert!(!got.is_alarm());
        assert!(matches!(got, CoreBinaryAttestation::SameVersion { .. }));
    }

    /// 🔴 **门：p101 实测现场必须被判为告警**。
    ///
    /// 现场（2026-07-29~31，SSH 别名 p101）：app 解析 `core_update/sing-box`（1.14.0-beta.3），
    /// helper 实跑 `/Library/Application Support/Polaris/core/sing-box`（1.14.0-alpha.45）。
    /// 缺陷期间零告警，用户看到的是「已连接 + 已升级」。
    ///
    /// **变异探针**：把 [`attest_core_binary`] 里 `rv == ev` 的分支改成恒返回 `SameVersion`
    /// （即"路径不同一律放行"），或把 [`CoreBinaryAttestation::is_alarm`] 的 `VersionMismatch`
    /// 去掉 → 本门转红。
    #[test]
    fn attest_p101_ground_truth_is_alarm() {
        let got = attest_core_binary(
            &p("/Users/sway/Library/Application Support/com.polaris.app/polaris/core_update/sing-box"),
            Some(&p("/Library/Application Support/Polaris/core/sing-box")),
            "sing-box version 1.14.0-beta.3",
            "sing-box version 1.14.0-alpha.45",
        );
        assert!(
            got.is_alarm(),
            "实跑 alpha.45 而期望 beta.3 必须告警，实得 {got:?}"
        );
        assert!(matches!(got, CoreBinaryAttestation::VersionMismatch { .. }));
        let msg = got.user_message();
        assert!(msg.contains("1.14.0-alpha.45") && msg.contains("1.14.0-beta.3"));
    }

    /// 路径不同 + 读不出版本 → 告警（不得当作通过）。
    ///
    /// **变异探针**：把 `VersionUnreadable` 从 [`CoreBinaryAttestation::is_alarm`] 里去掉 → 转红。
    #[test]
    fn attest_unreadable_version_is_alarm_not_pass() {
        for (rv, ev) in [("", "1.0.0"), ("1.0.0", ""), ("", "")] {
            let got = attest_core_binary(&p("/a/sing-box"), Some(&p("/b/sing-box")), ev, rv);
            assert!(
                got.is_alarm(),
                "读不出版本不得判通过（rv={rv:?} ev={ev:?}），实得 {got:?}"
            );
        }
    }

    /// 观测不到实跑 exe → 既不报通过也不报错。
    #[test]
    fn attest_unobservable_is_neither_pass_nor_alarm() {
        let got = attest_core_binary(&p("/a/sing-box"), None, "1.0.0", "1.0.0");
        assert_eq!(got, CoreBinaryAttestation::Unobservable);
        assert!(!got.is_alarm());
        assert!(
            !got.user_message().contains("通过"),
            "「没观测到」绝不能写成「通过」，实得 {}",
            got.user_message()
        );
    }

    // ── 暂存腿（真 FS，tempdir，无网络无提权）──

    #[test]
    fn stage_promote_dir_links_only_allowlisted_files() {
        let src = TmpDir::new();
        let staged = TmpDir::new();
        std::fs::write(src.path().join("sing-box"), b"CORE").unwrap();
        std::fs::write(src.path().join("sing-box.bak"), b"OLDCORE").unwrap();
        std::fs::write(src.path().join(".core-seed.json"), b"{}").unwrap();
        std::fs::write(src.path().join("libcronet.so"), b"CRONET").unwrap();

        let names = promote_names(&list_file_names(src.path()), "sing-box");
        let dest = staged.path().join(CORE_PROMOTE_DIR_NAME);
        stage_promote_dir(src.path(), &dest, &names).unwrap();

        let mut got = list_file_names(&dest);
        got.sort();
        assert_eq!(got, vec!["libcronet.so".to_owned(), "sing-box".to_owned()]);
        assert_eq!(std::fs::read(dest.join("sing-box")).unwrap(), b"CORE");
    }

    /// 暂存目录**先清后建**：上一轮残留不得混入下一次提升。
    ///
    /// **变异探针**：删掉 [`stage_promote_dir`] 里的 `remove_dir_all` → 本门转红。
    #[test]
    fn stage_promote_dir_wipes_stale_residue() {
        let src = TmpDir::new();
        let staged = TmpDir::new();
        std::fs::write(src.path().join("sing-box"), b"NEW").unwrap();
        let dest = staged.path().join(CORE_PROMOTE_DIR_NAME);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("libcronet.so"), b"STALE").unwrap();

        let names = promote_names(&list_file_names(src.path()), "sing-box");
        stage_promote_dir(src.path(), &dest, &names).unwrap();
        assert_eq!(
            list_file_names(&dest),
            vec!["sing-box".to_owned()],
            "上一轮的 libcronet.so 必须被清掉，否则会被 install-core 搬进 root 目录"
        );
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        let d = TmpDir::new();
        let f = d.path().join("x");
        std::fs::write(&f, b"").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(sha256_file(&d.path().join("nope")).is_err());
    }

    // ── 调用点守卫（源码扫描）──────────────────────────────────────────────
    //
    // 上面那组纯函数门只证明「判据本身对」。**判据对而没人调用 = 缺陷原样存活**，
    // 而这恰恰就是本缺陷的形态：helper 侧 `install-core` 早已完整落地并配着单测全绿，
    // 唯独 app 侧从头到尾没有一处调用它，于是换核一年也不生效。故必须把「腿还在不在、
    // 顺序对不对」单独钉死。
    //
    // 取材 `proxy.rs` 源码而非在 proxy.rs 里加测试：本轮改动要与另两个并行改 proxy.rs 的
    // 分支合并，测试收在本文件可把冲突面压到最小。

    /// 取 `proxy.rs` 里某方法的**自身**函数体（按花括号配对精确截断，不会漏到同 impl 的下一个方法）。
    fn proxy_method(sig: &str) -> &'static str {
        crate::runtime::core_update_scheduler::method_scan::method_body(
            include_str!("proxy.rs"),
            sig,
        )
    }

    /// 🔴 **门：经 helper 起核前必须先对账受保护核，且必须在 IPC 之前。**
    ///
    /// 顺序是硬要求：`install-core` 换的是 helper 下次 exec 的那个文件的内容，推晚一步
    /// （比如放到 `start_core` 之后）就要等**再下一次**起核才生效——用户点一次连接看不到变化，
    /// 与今天的症状肉眼无法区分。
    ///
    /// **变异探针**：删掉 `reconcile_protected_core(` 那一行 / 把它挪到 `start_core(` 之后 → 转红。
    #[test]
    fn helper_start_leg_reconciles_protected_core_before_ipc() {
        let body = proxy_method("    async fn spawn_core_via_helper(");
        let reconcile_at = body.find("reconcile_protected_core(").expect(
            "经 helper 起核前的受保护核对账被删了 —— helper 会继续 exec 它锁定路径上的旧核，\
             换核对 TUN 提权路径永久不生效（p101 实测：实跑 alpha.45 而期望 beta.3，持续一天多）",
        );
        let start_at = body.find("start_core(").expect("锚点消失：守卫已失去判据");
        assert!(
            reconcile_at < start_at,
            "对账必须在起核 IPC **之前** —— 放在之后，新核要等下一次起核才生效"
        );
    }

    /// 🔴 **门：`start_core` 不得再被传入核路径。**
    ///
    /// mac/win 的 `start` 协议没有核路径字段，helper 恒跑自己锁定的那个；早先调用方传了
    /// `&binary` 而 mac 分支**整个丢掉**，制造出「我请求了 A」的假象，正是本缺陷长期无人察觉的原因。
    ///
    /// **变异探针**：把 `start_core(&binary, &config_path, ...)` 改回去 → 转红。
    #[test]
    fn helper_start_never_passes_a_binary_path() {
        let body = proxy_method("    async fn spawn_core_via_helper(");
        assert!(
            body.contains("start_core(&config_path,"),
            "start_core 的首参必须是 config —— 传核路径会让调用方误以为 helper 跑的是它指定的核"
        );
        assert!(
            !body.contains("start_core(&binary"),
            "又把核路径传给 start_core 了：mac 分支会静默丢弃它，缺陷原样复现"
        );
    }

    /// 🔴 **门：核就绪后必须做实跑二进制自证，且在拿到 pid 之后。**
    ///
    /// 没有这一条，「helper 跑的不是我们要的核」就再次退回**零信号**状态——正是本次缺陷
    /// 一天多无人发现的根本原因（UI 一路显示已连接 + 已升级）。
    ///
    /// **变异探针**：删掉 `attest_running_core_binary(` 调用 → 转红。
    #[test]
    fn start_leg_attests_running_core_binary_after_ready() {
        let body = proxy_method("    async fn start_inner(");
        let attest_at = body.find("attest_running_core_binary(").expect(
            "起核后的实跑二进制自证被删了 —— 换核没生效将再次完全静默（UI 照旧显示已连接/已升级）",
        );
        let ready_at = body
            .find("CoreReadyOutcome::Ready")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            ready_at < attest_at,
            "自证必须在就绪门之后 —— 核还没起来时没有 pid 可观测"
        );
    }

    /// 🔴 **门：自证必须走 `set_nonfatal_error`（落状态 + 广播事件），不得退化成只打日志。**
    ///
    /// 「只 log::error」正是本仓 A1 腿踩过的坑：用户看到的是绿灯，日志在他看不见的地方喊。
    ///
    /// **变异探针**：把 `set_nonfatal_error` 换成 `log::error!` → 转红。
    #[test]
    fn core_binary_attestation_surfaces_via_nonfatal_error_channel() {
        let body = proxy_method("    async fn attest_running_core_binary(");
        assert!(
            body.contains("set_nonfatal_error(") && body.contains("code::CORE_BINARY_MISMATCH"),
            "自证告警必须经 set_nonfatal_error + CORE_BINARY_MISMATCH 落状态并广播，\
             只打日志等于用户看不到"
        );
        assert!(
            body.contains("is_alarm()"),
            "告警判定必须走 CoreBinaryAttestation::is_alarm（单一真值），别在此处另写一套分支"
        );
    }

    #[test]
    fn protected_core_path_is_platform_named() {
        assert_eq!(
            protected_core_path_in(Path::new("/c"), "windows"),
            p("/c/sing-box.exe")
        );
        assert_eq!(
            protected_core_path_in(Path::new("/c"), "macos"),
            p("/c/sing-box")
        );
    }
}
