//! install-core —— linux 薄壳：公共核心下沉 [`crate::core_install`]，本文件
//! 只留 linux 平台 hook（保守 lib*.so 清理）。
//!
//! ## 公共核心（已下沉）
//!
//! sha256 校验 / 枚举源目录 / 逐文件原子安装 一律走 [`crate::core_install`]，
//! 结果类型直接用公共 [`InstallResult`]。mac/linux 两份原本逐字同，合并后单一真值。
//!
//! 原先此处另有个 `InstallOutcome` 影子枚举：与 [`InstallResult`] **10 个 variant 逐一同构**
//!（唯一差异是拼写 `CoredirUnset` vs `CoreDirUnset`，纯历史命名不一致，wire 输出本就相同），
//! 外加 `to_common`/`from_common` 双向映射把公共枚举翻来覆去搬一遍。影子枚举 + 双射已删（G3.4）：
//! `install_core` 直接返回 `InstallResult`，各步的 `Err(e) => return e` 一步到位。
//!
//! ## linux 专属（本 crate 保留）
//!
//! - **保守 prune**：linux 仅清 `sing-box` / `lib*` 前缀残留（防误删同目录 helper 二进制），
//!   不用公共通用版 `prune_extra_files`（那个会删 keep_names 外的所有文件）。此安全语义为
//!   linux 专属，保留本 crate。
//!
//! ## 安全模型（对照 Go 源注释 :9-18）
//!
//! - 核二进制在 root-owned 受管目录（coreDir），普通用户改不动 → 杜绝「借 helper 给任意自有二进制赋 CAP_NET_ADMIN」。
//! - start 只跑锁定的 `coreDir/sing-box`，绝不跑客户端指定的任意路径（见 handler.rs 的 start 路径锁）。
//! - install-core 校验 `sha256(srcDir/sing-box) == wantHash` 后，逐文件 `.new + rename` 原子就位（防 TOCTOU）。
//! - 只写 coreDir、不接受任意路径；清陈旧残留仅删核配套（sing-box / lib*.so*），不碰 helper 自身二进制。

use std::fs;
use std::path::Path;

// 公共核心走同 crate 的共用层（合并前是 `pub use polaris_helper_common::core_install::{...}` 的
// 转发块 —— 单 crate 后转发已无意义，降为普通 `use`，不再对外重复导出一份公共符号）。
use crate::core_install::{
    atomic_install_files, list_src_files, verify_singbox_hash, InstallResult, SINGBOX_BIN_NAME,
};
use polaris_helper_proto::codec::is_valid_sha256_hex;

/// 执行 install-core（移植自 Go `installCore`，:183-244）。
///
/// 参数：
/// - `core_dir`：锁定的 root-owned 受管核目录（None = 未配置 → `ERR coredir-unset`）。
/// - `src_dir`：app 下载+预检的临时核源目录。
/// - `want_hash`：期望的 sing-box sha256（hex，64 字符）。
///
/// 文件操作：建 coreDir → 读 srcDir/sing-box 校验 sha256 → 逐文件 `.new + rename` 原子就位 →
/// 清陈旧残留（linux 保守策略：仅 sing-box / lib* 前缀）。公共核心步骤走 helper-common，
/// 本函数只加参数适配 + linux 专属保守 prune。
pub fn install_core(core_dir: Option<&Path>, src_dir: &str, want_hash: &str) -> InstallResult {
    // :184-185: coreDir 未配置。
    let Some(core_dir) = core_dir else {
        return InstallResult::CoreDirUnset;
    };
    // :187-188: bad-args —— srcDir 空 或 wantHash 非 64 hex 字符。
    if src_dir.is_empty() || !is_valid_sha256_hex(want_hash) {
        return InstallResult::BadArgs;
    }
    let src = Path::new(src_dir);

    // 公共核心：校验 sing-box 哈希（:190-196）。各 Err 已是 InstallResult，原样返回。
    let sb_data = match verify_singbox_hash(src, want_hash) {
        Ok(d) => d,
        Err(e) => return e,
    };

    // 公共核心：枚举源目录（:198-200）。
    let names = match list_src_files(src) {
        Ok(n) => n,
        Err(e) => return e,
    };

    // 公共核心：逐文件 .new + rename 原子就位（:202-228，含 MkdirAll coreDir）。
    if let Err(e) = atomic_install_files(src, core_dir, &names, &sb_data) {
        return e;
    }

    // :230-242: linux 专属保守 prune —— 仅清 sing-box / lib* 前缀残留（防误删同目录 helper 二进制）。
    // 不走公共通用 prune_extra_files（那个会删 keep_names 外的全部文件，对 linux 太宽）。
    prune_lib_and_singbox(core_dir, &names);

    InstallResult::Installed
}

/// 清 coreDir 里非本次 srcDir 的旧核配套残留（rollback 后陈旧 lib*.so 等）—— linux 保守策略。
///
/// 仅删 `sing-box` 或 `lib*` 前缀（Go: `n == "sing-box" || strings.HasPrefix(n, "lib")`）；
/// 保留本次 src_names 命中的；跳过子目录与 .new 临时件。best-effort，单项失败跳过。
fn prune_lib_and_singbox(core_dir: &Path, src_names: &[String]) {
    let Ok(old) = fs::read_dir(core_dir) else {
        return;
    };
    for ent_res in old {
        let Ok(ent) = ent_res else { continue };
        let Ok(ft) = ent.file_type() else { continue };
        if ft.is_dir() {
            continue;
        }
        let n = ent.file_name().to_string_lossy().into_owned();
        // 保留本次安装的 + .new 临时件（上方刚 rename 完，理论上无 .new 残留）。
        if src_names.iter().any(|s| s == &n) || n.ends_with(".new") {
            continue;
        }
        // 仅清核配套：sing-box 或 lib* 前缀（Go: n == "sing-box" || strings.HasPrefix(n, "lib")）。
        if n == SINGBOX_BIN_NAME || n.starts_with("lib") {
            let _ = fs::remove_file(core_dir.join(&n));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use std::fs::write;
    use tempfile::tempdir;

    /// 造一个 srcDir：sing-box + libcronet.so 等配套（对照 Go 注释 :182 的真实核形态）。
    fn make_src_dir(dir: &Path, singbox: &[u8], extras: &[(&str, &[u8])]) {
        write(dir.join(SINGBOX_BIN_NAME), singbox).unwrap();
        // 设可执行权限（生产核是可执行二进制）。
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join(SINGBOX_BIN_NAME),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for (name, data) in extras {
            write(dir.join(name), data).unwrap();
        }
    }

    /// 测试本地 sha256（与生产的 sha256_hex 独立实现，避免循环依赖断言）。
    fn sha256_hex_local(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(data);
        hex::encode(h.finalize())
    }

    #[test]
    fn coredir_unset_returns_err() {
        // Go :184-185: coreDir == "" → "ERR coredir-unset"
        let r = install_core(None, "/tmp/src", &"a".repeat(64));
        assert_eq!(r, InstallResult::CoreDirUnset);
        assert_eq!(r.to_wire_line(), "ERR coredir-unset");
    }

    #[test]
    fn bad_args_when_src_empty() {
        let r = install_core(Some(Path::new("/tmp/core")), "", &"a".repeat(64));
        assert_eq!(r, InstallResult::BadArgs);
    }

    #[test]
    fn bad_args_when_hash_wrong_length() {
        // Go :187: len(wantHash) != 64 → bad-args
        let r = install_core(Some(Path::new("/tmp/core")), "/tmp/src", "abc");
        assert_eq!(r, InstallResult::BadArgs);
    }

    #[test]
    fn bad_args_when_hash_not_hex() {
        // 64 字符但非 hex（proto crate 的 is_valid_sha256_hex 拒绝）。
        let r = install_core(Some(Path::new("/tmp/core")), "/tmp/src", &"z".repeat(64));
        assert_eq!(r, InstallResult::BadArgs);
    }

    #[test]
    fn read_singbox_failure_when_missing() {
        // srcDir 存在但无 sing-box 文件。
        let src = tempdir().unwrap();
        let core = tempdir().unwrap();
        let r = install_core(
            Some(core.path()),
            src.path().to_str().unwrap(),
            &"a".repeat(64),
        );
        let wire = r.to_wire_line();
        match r {
            // io::Error 详情不含 "sing-box"（是 OS 错误文本），只验证变体命中。
            InstallResult::ReadSingbox(_) => {}
            other => panic!("expected ReadSingbox, got {other:?}"),
        }
        assert!(
            wire.starts_with("ERR read-singbox"),
            "wire 应以 ERR read-singbox 开头，got {wire}"
        );
    }

    #[test]
    fn hash_mismatch_when_content_changed() {
        let src = tempdir().unwrap();
        let core = tempdir().unwrap();
        make_src_dir(src.path(), b"real-sing-box-binary", &[]);
        // 故意给错 hash（内容对应的真实 hash 与 wantHash 不符）。
        let r = install_core(
            Some(core.path()),
            src.path().to_str().unwrap(),
            &"0".repeat(64),
        );
        assert_eq!(r, InstallResult::HashMismatch);
        assert_eq!(r.to_wire_line(), "ERR hash-mismatch");
    }

    #[test]
    fn successful_install_copies_singbox_and_extras() {
        let src = tempdir().unwrap();
        let core = tempdir().unwrap();
        let sb = b"#!bin\nsing-box-binary-v1.2.3";
        let cronet = b"cronet shared lib bytes";
        make_src_dir(src.path(), sb, &[("libcronet.so", cronet)]);
        let want_hash = sha256_hex_local(sb);
        let r = install_core(Some(core.path()), src.path().to_str().unwrap(), &want_hash);
        assert_eq!(r, InstallResult::Installed);
        assert!(r.is_ok());
        assert_eq!(r.to_wire_line(), "OK installed");
        // 校验落盘内容。
        assert_eq!(
            std::fs::read(core.path().join(SINGBOX_BIN_NAME)).unwrap(),
            sb
        );
        assert_eq!(
            std::fs::read(core.path().join("libcronet.so")).unwrap(),
            cronet
        );
        // 可执行权限校验（0755）。
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(core.path().join(SINGBOX_BIN_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "sing-box 须 0755 可执行");
    }

    #[test]
    fn hash_case_insensitive_match() {
        // Go strings.EqualFold 大小写不敏感。wantHash 大写也应匹配小写计算结果。
        let src = tempdir().unwrap();
        let core = tempdir().unwrap();
        let sb = b"binary";
        make_src_dir(src.path(), sb, &[]);
        let want_upper = sha256_hex_local(sb).to_uppercase();
        let r = install_core(Some(core.path()), src.path().to_str().unwrap(), &want_upper);
        assert_eq!(r, InstallResult::Installed);
    }

    #[test]
    fn install_clears_stale_core_artifacts() {
        // coreDir 里残留旧 sing-box / libfoo.so，本次 srcDir 不含它们 → 应被清。
        let src = tempdir().unwrap();
        let core = tempdir().unwrap();
        // 预置陈旧残留。
        write(core.path().join(SINGBOX_BIN_NAME), b"OLD singbox").unwrap();
        write(core.path().join("libstale.so"), b"stale lib").unwrap();
        write(core.path().join("helper.bin"), b"helper self").unwrap(); // 非核文件，应保留

        let sb = b"NEW singbox";
        make_src_dir(src.path(), sb, &[("libnew.so", b"new lib")]);
        let want = sha256_hex_local(sb);
        let r = install_core(Some(core.path()), src.path().to_str().unwrap(), &want);
        assert_eq!(r, InstallResult::Installed);
        // 旧 sing-box 被新内容覆盖。
        assert_eq!(
            std::fs::read(core.path().join(SINGBOX_BIN_NAME)).unwrap(),
            b"NEW singbox"
        );
        // 旧 libstale.so 被清（lib* 前缀）。
        assert!(
            !core.path().join("libstale.so").exists(),
            "旧 lib*.so 应被清"
        );
        // 新 libnew.so 在位。
        assert!(core.path().join("libnew.so").exists());
        // helper.bin 保留（非 sing-box / 非 lib* 前缀）。
        assert!(
            core.path().join("helper.bin").exists(),
            "非核文件不应被误删"
        );
    }

    #[test]
    fn install_skips_subdirectories_in_src() {
        // Go :209-210: if e.IsDir() { continue } —— srcDir 里的子目录不复制。
        let src = tempdir().unwrap();
        let core = tempdir().unwrap();
        let sb = b"bin";
        make_src_dir(src.path(), sb, &[]);
        // 建一个子目录（应被跳过）。
        std::fs::create_dir(src.path().join("subdir")).unwrap();
        let want = sha256_hex_local(sb);
        let r = install_core(Some(core.path()), src.path().to_str().unwrap(), &want);
        assert_eq!(r, InstallResult::Installed);
        assert!(!core.path().join("subdir").exists(), "子目录不应被复制");
    }

    #[test]
    fn read_dir_failure_when_src_missing() {
        let core = tempdir().unwrap();
        // srcDir 不存在 → read sing-box 会先报错（Go 顺序是先 read sing-box :190 再 read_dir :198）。
        let r = install_core(
            Some(core.path()),
            "/nonexistent/src/dir/xyz",
            &"a".repeat(64),
        );
        // read sing-box 先失败（srcDir/sing-box 不存在）。
        match r {
            InstallResult::ReadSingbox(_) => {}
            other => panic!("expected ReadSingbox for missing srcDir, got {other:?}"),
        }
    }

    #[test]
    fn wire_line_for_all_outcomes() {
        // 锁住 wire 形态（对照 Go 源各 return 分支的字符串；委托公共 to_wire_line，输出应逐字同）。
        assert_eq!(InstallResult::Installed.to_wire_line(), "OK installed");
        assert_eq!(
            InstallResult::CoreDirUnset.to_wire_line(),
            "ERR coredir-unset"
        );
        assert_eq!(InstallResult::BadArgs.to_wire_line(), "ERR bad-args");
        assert_eq!(
            InstallResult::HashMismatch.to_wire_line(),
            "ERR hash-mismatch"
        );
        assert_eq!(
            InstallResult::ReadSingbox("e".into()).to_wire_line(),
            "ERR read-singbox e"
        );
        assert_eq!(
            InstallResult::ReadDir("e".into()).to_wire_line(),
            "ERR readdir e"
        );
        assert_eq!(
            InstallResult::Mkdir("e".into()).to_wire_line(),
            "ERR mkdir e"
        );
        assert_eq!(
            InstallResult::Read {
                name: "libcronet.so".into(),
                detail: "perm".into()
            }
            .to_wire_line(),
            "ERR read libcronet.so perm"
        );
        assert_eq!(
            InstallResult::Write {
                name: "x".into(),
                detail: "full".into()
            }
            .to_wire_line(),
            "ERR write x full"
        );
        assert_eq!(
            InstallResult::Rename {
                name: "y".into(),
                detail: "busy".into()
            }
            .to_wire_line(),
            "ERR rename y busy"
        );
    }

    /// 静态断言：is_ok 仅对 Installed 真。
    #[test]
    fn is_ok_predicate() {
        assert!(InstallResult::Installed.is_ok());
        assert!(!InstallResult::BadArgs.is_ok());
        assert!(!InstallResult::HashMismatch.is_ok());
    }
}
