//! install-core 公共核心（mac/linux 逐字等价部分下沉）。
//!
//! 把 app 下载+预检的临时内核 src，校验 sha256 后 root 写入锁定的受保护目录 coreDir +
//! 逐文件 `.new + rename` 原子就位 + 清陈旧残留。移植自 上游 `helper/helper.go:127-198`
//!（mac）与 `helper-linux/helper.go:183-244`（linux），两份核心文件操作逐字同。
//!
//! ## 职责边界
//!
//! 只放**跨平台逐字等价**的逻辑：sha256 校验、原子安装、通用 prune。OS 专属差异
//! （mac xattr/codesign、linux lib*.so 限定清理）由各 helper crate 调本模块前后 hook。
//!
//! ## 安全约束（逐字对照 Go 源）
//!
//! - **只写锁定的 coreDir**（`filepath.Join`，不接受任意路径）→ 防「持 token 写任意 root 路径」。
//! - **哈希校验主二进制 sing-box**：读全字节进内存校验（堵 TOCTOU：攻击者读后替换无效，读前替换则 hash
//!   不符被拒）。与 token（通道鉴权）互补两层。
//!
//! ## 移植纪律
//!
//! Go 源的「整个源目录逐文件 .new + rename 原子就位」+「清受保护目录多余文件」在本模块拆为：
//! - [`sha256_hex`]：sha256 → hex（纯函数，helper-common 内部 + verify 复用）。
//! - [`verify_singbox_hash`]：纯逻辑 sha256 校验（读文件 + sha256 + 比对）。
//! - [`list_src_files`]：枚举源目录文件（fs 操作）。
//! - [`atomic_install_files`]：逐文件 .new + rename + chmod（fs）。
//! - [`prune_extra_files`]：清受保护目录多余旧残留（通用版，keep_names 外全删，best-effort）。
//! - [`install_core_files`]：完整流程编排（参数校验 + 上四步），平台 hook 留各 helper crate。

use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// sing-box 主二进制文件名（`helper.go:140,194`：`filepath.Join(srcDir, "sing-box")`）。
pub const SINGBOX_BIN_NAME: &str = "sing-box";

/// install-core 结果（统一命名，融合 mac `InstallResult` 与 linux `InstallOutcome`，二者同构）。
///
/// 对照 Go `installCore` 的所有 return 分支（`helper.go:133-198` / `helper-linux/helper.go:183-244`）。
/// wire 序列化见 [`InstallResult::to_wire_line`]，输出格式与原 mac/linux 逐字一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallResult {
    /// `OK installed`（mac `helper.go:197` / linux `:243`）。
    Installed,
    /// `ERR coredir-unset`（mac `helper.go:135` / linux `:185`：coreDir 未配置）。
    CoreDirUnset,
    /// `ERR bad-args`（mac `helper.go:137` / linux `:187`：srcDir 空 或 wantHash 长度 != 64）。
    BadArgs,
    /// `ERR read-singbox <err>`（mac `helper.go:142` / linux `:192`：读主二进制失败）。
    ReadSingbox(String),
    /// `ERR hash-mismatch`（mac `helper.go:146` / linux `:196`：sha256 不符）。
    HashMismatch,
    /// `ERR readdir <err>`（mac `helper.go:149` / linux `:199`：枚举源目录失败）。
    ReadDir(String),
    /// `ERR mkdir <err>`（mac `helper.go:153` / linux `:203`：创建 coreDir 失败）。
    Mkdir(String),
    /// `ERR read <name> <err>`（mac `helper.go:164` / linux `:215`：读配套文件失败）。
    Read { name: String, detail: String },
    /// `ERR write <name> <err>`（mac `helper.go:171` / linux `:221`：写 .new 失败）。
    Write { name: String, detail: String },
    /// `ERR rename <name> <err>`（mac `helper.go:174` / linux `:225`：rename 失败）。
    Rename { name: String, detail: String },
}

impl InstallResult {
    /// 转 wire 响应行（对照 Go `installCore` 的 return 字符串，handler 直接写出）。
    ///
    /// 输出格式与原 mac `From<InstallResult> for Response` 走 `ProtoError::with_detail` 的
    /// `read-singbox <d>` / `readdir <d>` / ... 逐字一致，也与原 linux `InstallOutcome::to_wire_line`
    /// 同构 —— 二者均源自同一 Go 源，故 wire 协议统一后无需迁移。
    #[must_use]
    pub fn to_wire_line(&self) -> String {
        match self {
            Self::Installed => "OK installed".to_string(),
            Self::CoreDirUnset => "ERR coredir-unset".to_string(),
            Self::BadArgs => "ERR bad-args".to_string(),
            Self::ReadSingbox(d) => format!("ERR read-singbox {d}"),
            Self::HashMismatch => "ERR hash-mismatch".to_string(),
            Self::ReadDir(d) => format!("ERR readdir {d}"),
            Self::Mkdir(d) => format!("ERR mkdir {d}"),
            Self::Read { name, detail } => format!("ERR read {name} {detail}"),
            Self::Write { name, detail } => format!("ERR write {name} {detail}"),
            Self::Rename { name, detail } => format!("ERR rename {name} {detail}"),
        }
    }

    /// 是否成功（便于上层短路，对齐原 linux `InstallOutcome::is_ok`）。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Installed)
    }
}

/// sha256 → 小写 hex（对照 Go `sha256.Sum256` + `hex.EncodeToString`）。
///
/// 纯函数，[`verify_singbox_hash`] 内部复用，也供 helper-common 外部测试断言用。
/// 合并自原 linux `sha256_hex`（mac 原为内联 `Sha256::new` + `hex::encode`，等价）。
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 校验 sing-box 主二进制 sha256（移植自 `helper.go:140-147`）。
///
/// 读 src_dir/sing-box 全字节，计算 sha256 与 want_hash 比对（大小写不敏感，对齐 Go `EqualFold`）。
/// 返回读到的字节（供后续原子写入复用，堵 TOCTOU —— 见 Go `helper.go:161`：`data := sbData`）。
pub fn verify_singbox_hash(src_dir: &Path, want_hash: &str) -> Result<Vec<u8>, InstallResult> {
    let sb_path = src_dir.join(SINGBOX_BIN_NAME);
    let sb_data = fs::read(&sb_path).map_err(|e| InstallResult::ReadSingbox(e.to_string()))?;
    // helper.go:144-146: sha256.Sum256 + hex + EqualFold
    let actual = sha256_hex(&sb_data);
    if !actual.eq_ignore_ascii_case(want_hash) {
        return Err(InstallResult::HashMismatch);
    }
    Ok(sb_data)
}

/// 枚举源目录的文件条目（移植自 `helper.go:148-151` 的 `os.ReadDir`）。
///
/// 返回所有**非目录**条目名（按字母序，对齐 Go ReadDir 的排序）。目录条目跳过（`helper.go:158`）。
pub fn list_src_files(src_dir: &Path) -> Result<Vec<String>, InstallResult> {
    let entries = fs::read_dir(src_dir).map_err(|e| InstallResult::ReadDir(e.to_string()))?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            // helper.go:158: if e.IsDir() { continue }
            let ft = e.file_type().ok()?;
            if ft.is_dir() {
                None
            } else {
                Some(e.file_name().to_string_lossy().into_owned())
            }
        })
        .collect();
    names.sort(); // Go os.ReadDir 返回已排序
    Ok(names)
}

/// 逐文件原子写入受保护目录（移植自 `helper.go:156-178`）。
///
/// 对照 Go：
/// ```text
/// for _, e := range entries {
///     if e.IsDir() { continue }
///     name := e.Name()
///     data := sbData          // sing-box 复用已校验字节
///     if name != "sing-box" { data = read(srcDir/name) }
///     dst := Join(coreDir, name); tmp := dst + ".new"
///     WriteFile(tmp, data, 0755)
///     Rename(tmp, dst)
///     Chmod(dst, 0755)
/// }
/// ```
///
/// `sing-box` 用传入的 `sb_data`（已校验字节，堵 TOCTOU）；其它文件从 src_dir 读。
/// tmp 路径构造逐字对照 Go `dst + ".new"`（`helper-linux/helper.go:218-228`）—— 直接在 dst
/// 后缀 `.new`，而非 mac 原版的「set_extension 替换扩展名」（原 mac 实现对无扩展名的 `sing-box`
/// 会变成 `sing-box.new` 而非 `sing-box.new`，二者结果一致；但对带扩展名的 `libcronet.dylib`
/// mac 原版产出 `libcronet.dylib.new` 也是对的——与 linux 同）。统一采用 linux 的「append .new」
/// 形式以更贴合 Go 源且更易读。
pub fn atomic_install_files(
    src_dir: &Path,
    core_dir: &Path,
    names: &[String],
    sb_data: &[u8],
) -> Result<(), InstallResult> {
    // helper.go:152-154: MkdirAll(coreDir, 0755)
    fs::create_dir_all(core_dir).map_err(|e| InstallResult::Mkdir(e.to_string()))?;

    for name in names {
        let dst = core_dir.join(name);
        // helper.go:161-166: sing-box 复用 sbData，否则读 src/name
        let data: Vec<u8> = if name == SINGBOX_BIN_NAME {
            sb_data.to_vec()
        } else {
            fs::read(src_dir.join(name)).map_err(|e| InstallResult::Read {
                name: name.clone(),
                detail: e.to_string(),
            })?
        };
        // tmp = dst + ".new"（对照 linux `helper-linux/helper.go:218-228` 与 mac 原版语义）。
        let tmp = {
            let mut t = dst.as_os_str().to_os_string();
            t.push(".new");
            std::path::PathBuf::from(t)
        };
        // helper.go:170-172: WriteFile(tmp, data, 0755)
        fs::write(&tmp, &data).map_err(|e| InstallResult::Write {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        // helper.go:173-176: Rename(tmp, dst)；失败清 tmp
        if let Err(e) = fs::rename(&tmp, &dst) {
            let _ = fs::remove_file(&tmp);
            return Err(InstallResult::Rename {
                name: name.clone(),
                detail: e.to_string(),
            });
        }
        // helper.go:177: Chmod(dst, 0755)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
        }
    }
    Ok(())
}

/// 清理受保护目录多余文件（移植自 `helper.go:179-192`，通用版）。
///
/// 删除 core_dir 中不在 keep_names 的旧文件（防 rollback 后残留陈旧配套）。
/// best-effort，单项失败跳过。linux 专属的「仅删 lib*.so* / sing-box」保守策略由各 helper
/// crate 自行在调用本函数后追加（或用更窄的 keep_names 配合）。
pub fn prune_extra_files(core_dir: &Path, keep_names: &[String]) {
    // helper.go:186-192: ReadDir(coreDir)，非目录且不在 keep_names 的删除
    let Ok(entries) = fs::read_dir(core_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep_names.contains(&name) {
            // helper.go:189: _ = os.Remove —— 失败忽略
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 完整 install-core 流程编排（不含 mac 专属 xattr/codesign、linux 专属 lib*.so 清理 ——
/// 那两步由各 helper crate 在调用本函数前后 hook）。
///
/// 严格对照 Go `installCore`(`helper.go:133-198`) 的文件操作部分：
/// 1. 参数校验（core_dir 非空、src_dir 非空、want_hash 64 字符 hex）
/// 2. 校验 sing-box sha256
/// 3. 枚举源目录文件
/// 4. mkdir core_dir
/// 5. 逐文件原子写入
/// 6. 清理多余旧文件（通用 prune）
///
/// 返回 `(InstallResult, names)`：names 为本次安装的源目录文件名列表（供平台 hook 用，
/// 如 linux 可据此判定哪些保留、哪些是 lib*.so* 残留需清）。成功时 result 为 [`InstallResult::Installed`]。
pub fn install_core_files(
    core_dir: &Path,
    src_dir: &Path,
    want_hash: &str,
) -> Result<Vec<String>, InstallResult> {
    // helper.go:134-138: 参数校验
    if core_dir.as_os_str().is_empty() {
        return Err(InstallResult::CoreDirUnset);
    }
    if src_dir.as_os_str().is_empty() || !is_valid_sha256_hex(want_hash) {
        return Err(InstallResult::BadArgs);
    }
    // helper.go:140-147: 校验 sing-box 哈希
    let sb_data = verify_singbox_hash(src_dir, want_hash)?;
    // helper.go:148-151: 枚举源目录
    let names = list_src_files(src_dir)?;
    // helper.go:156-178: 逐文件原子写入
    atomic_install_files(src_dir, core_dir, &names, &sb_data)?;
    // helper.go:179-192: 清理多余旧文件
    prune_extra_files(core_dir, &names);
    Ok(names)
}

/// 便利：判断给定路径是否可作为 core_dir（非空 + 父目录存在）。
#[must_use]
pub fn is_valid_core_dir(core_dir: &Path) -> bool {
    !core_dir.as_os_str().is_empty() && core_dir.parent().is_some_and(|p| p.exists())
}

// want_hash 校验（64 字符 hex，大小写不敏感）：直接用 proto 的单一真值
// [`polaris_helper_proto::codec::is_valid_sha256_hex`]，逐字对照 Go `len(wantHash) != 64`。
//
// 合并前此处内联过一份等价实现，自辩理由是「本 crate（helper-common）不依赖 proto」。三平台 helper
// 合并成单 crate 后 helper-common 已不存在，本 crate **确实依赖 proto**（mac/win/linux 三支本来就都
// 依赖）—— 那条理由随之失效，留着内联副本反而让注释变成谎话（正是审计 §G1.3 刚消灭的那种误导注释）。
// 且同 crate 内 linux 那支（platform::linux::core_installer）本就用的是 proto 这份，副本使一个 crate
// 里同时存在两份同语义校验。故删副本、统一到 proto。
//
// 注：这不动 helper-proto 的 `[dependencies]`（仍为空）—— 只是消费既有的 helper → proto 边。
use polaris_helper_proto::codec::is_valid_sha256_hex;

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;

    /// 准备一个临时源目录：sing-box + 一个配套文件，返回 (dir, sha256_hex)。
    fn make_src_dir() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let sb_content = b"fake sing-box binary content";
        fs::write(dir.path().join(SINGBOX_BIN_NAME), sb_content).unwrap();
        // 给 sing-box 可执行权限（unix）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                dir.path().join(SINGBOX_BIN_NAME),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        fs::write(dir.path().join("libcronet.dylib"), b"fake libcronet").unwrap();

        let hash = sha256_hex(sb_content);
        (dir, hash)
    }

    // ===== sha256_hex（合并自 linux 自实现 + mac 内联） =====

    #[test]
    fn sha256_hex_known_vector() {
        // sha256(b"") = e3b0c4...
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256(b"abc") = ba7816...
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_hex_lowercase() {
        // Go hex.EncodeToString 输出小写
        let h = sha256_hex(b"X");
        assert_eq!(h, h.to_lowercase());
        assert!(!h.chars().any(|c| c.is_ascii_uppercase()));
    }

    // ===== verify_singbox_hash（移植自 mac 单测） =====

    #[test]
    fn verify_singbox_hash_matches() {
        // helper.go:144-146: sha256 匹配
        let (dir, hash) = make_src_dir();
        let result = verify_singbox_hash(dir.path(), &hash);
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn verify_singbox_hash_case_insensitive() {
        // Go EqualFold 大小写不敏感
        let (dir, hash) = make_src_dir();
        let upper = hash.to_uppercase();
        assert!(verify_singbox_hash(dir.path(), &upper).is_ok());
    }

    #[test]
    fn verify_singbox_hash_mismatch() {
        // helper.go:146: hash 不符
        let (dir, _hash) = make_src_dir();
        let wrong = "0".repeat(64);
        let result = verify_singbox_hash(dir.path(), &wrong);
        assert_eq!(result.unwrap_err(), InstallResult::HashMismatch);
    }

    #[test]
    fn verify_singbox_read_fails() {
        // helper.go:142: 读失败 → ERR read-singbox
        let dir = tempfile::tempdir().unwrap(); // 空目录，无 sing-box
        let result = verify_singbox_hash(dir.path(), &"a".repeat(64));
        assert!(matches!(result, Err(InstallResult::ReadSingbox(_))));
    }

    // ===== list_src_files（移植自 mac 单测） =====

    #[test]
    fn list_src_files_excludes_dirs() {
        // helper.go:158: if e.IsDir() { continue }
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("sing-box"), b"x").unwrap();
        fs::write(dir.path().join("libcronet.dylib"), b"y").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        let names = list_src_files(dir.path()).unwrap();
        assert!(names.contains(&"sing-box".to_owned()));
        assert!(names.contains(&"libcronet.dylib".to_owned()));
        assert!(!names.contains(&"subdir".to_owned()), "目录应被排除");
    }

    #[test]
    fn list_src_files_sorted() {
        // Go os.ReadDir 返回已排序
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("zzz"), b"1").unwrap();
        fs::write(dir.path().join("aaa"), b"2").unwrap();
        fs::write(dir.path().join("mmm"), b"3").unwrap();
        let names = list_src_files(dir.path()).unwrap();
        assert_eq!(names, vec!["aaa", "mmm", "zzz"]);
    }

    // ===== atomic_install_files（移植自 mac 单测） =====

    #[test]
    fn atomic_install_writes_all_files() {
        // helper.go:156-178: 逐文件原子写入 + chmod 0755
        let (src, hash) = make_src_dir();
        let core = tempfile::tempdir().unwrap();
        let sb_data = verify_singbox_hash(src.path(), &hash).unwrap();
        let names = list_src_files(src.path()).unwrap();
        atomic_install_files(src.path(), core.path(), &names, &sb_data).unwrap();

        // 验证两文件都就位
        assert!(core.path().join(SINGBOX_BIN_NAME).exists());
        assert!(core.path().join("libcronet.dylib").exists());
        // sing-box 内容与源一致（堵 TOCTOU：用的是已校验字节）
        let installed = fs::read(core.path().join(SINGBOX_BIN_NAME)).unwrap();
        assert_eq!(installed, b"fake sing-box binary content");
        // 权限 0755（unix）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(core.path().join(SINGBOX_BIN_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn atomic_install_leaves_no_tmp_files() {
        // .new 文件应已 rename 掉，不留残留
        let (src, hash) = make_src_dir();
        let core = tempfile::tempdir().unwrap();
        let sb_data = verify_singbox_hash(src.path(), &hash).unwrap();
        let names = list_src_files(src.path()).unwrap();
        atomic_install_files(src.path(), core.path(), &names, &sb_data).unwrap();
        // 不应有 .new 文件残留
        let entries: Vec<_> = fs::read_dir(core.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!entries.iter().any(|n| n.ends_with(".new")));
    }

    #[test]
    fn atomic_install_mkdirs_core_dir() {
        // core_dir 不存在时应自动建（helper.go:152-154 MkdirAll）
        let (src, hash) = make_src_dir();
        let parent = tempfile::tempdir().unwrap();
        let core = parent.path().join("nested").join("core");
        let sb_data = verify_singbox_hash(src.path(), &hash).unwrap();
        let names = list_src_files(src.path()).unwrap();
        atomic_install_files(src.path(), &core, &names, &sb_data).unwrap();
        assert!(core.join(SINGBOX_BIN_NAME).exists());
    }

    // ===== prune_extra_files（移植自 mac 单测） =====

    #[test]
    fn prune_extra_files_removes_old() {
        // helper.go:179-192: 清理不在 keep_names 的旧文件
        let core = tempfile::tempdir().unwrap();
        // 模拟旧残留：旧版 sing-box + 旧配套
        fs::write(core.path().join("sing-box"), b"old").unwrap();
        fs::write(core.path().join("libcronet_old.dylib"), b"old dylib").unwrap();
        fs::write(core.path().join("stale.bin"), b"stale").unwrap();
        // 新 src 只有 sing-box + libcronet.dylib
        let keep = vec!["sing-box".to_owned(), "libcronet.dylib".to_owned()];
        prune_extra_files(core.path(), &keep);
        // 旧残留应被删
        assert!(!core.path().join("stale.bin").exists());
        assert!(!core.path().join("libcronet_old.dylib").exists());
        // sing-box 保留（在 keep 中）
        assert!(core.path().join("sing-box").exists());
    }

    #[test]
    fn prune_extra_files_missing_dir_is_noop() {
        // core_dir 不存在 → 静默 no-op（best-effort）
        prune_extra_files(Path::new("/nonexistent/xyz/core"), &[]);
    }

    // ===== install_core_files 完整流程（移植自 mac 单测） =====

    #[test]
    fn install_core_files_full_flow() {
        // 完整流程：校验 → 枚举 → 写入 → 清理
        let (src, hash) = make_src_dir();
        let core = tempfile::tempdir().unwrap();
        // 预放一个旧残留
        fs::write(core.path().join("stale.bin"), b"stale").unwrap();

        let result = install_core_files(core.path(), src.path(), &hash);
        assert!(result.is_ok());
        // 旧残留被清
        assert!(!core.path().join("stale.bin").exists());
        // 新文件就位
        assert!(core.path().join(SINGBOX_BIN_NAME).exists());
        assert!(core.path().join("libcronet.dylib").exists());
    }

    #[test]
    fn install_core_files_coredir_unset() {
        // helper.go:135: coreDir 空 → ERR coredir-unset
        let (src, hash) = make_src_dir();
        let result = install_core_files(Path::new(""), src.path(), &hash);
        assert_eq!(result.unwrap_err(), InstallResult::CoreDirUnset);
    }

    #[test]
    fn install_core_files_bad_args_empty_src() {
        // helper.go:137: srcDir 空 → ERR bad-args
        let core = tempfile::tempdir().unwrap();
        let result = install_core_files(core.path(), Path::new(""), &"a".repeat(64));
        assert_eq!(result.unwrap_err(), InstallResult::BadArgs);
    }

    #[test]
    fn install_core_files_bad_args_short_hash() {
        // helper.go:137: wantHash 长度 != 64 → ERR bad-args
        let (src, _hash) = make_src_dir();
        let core = tempfile::tempdir().unwrap();
        let result = install_core_files(core.path(), src.path(), "abc");
        assert_eq!(result.unwrap_err(), InstallResult::BadArgs);
    }

    #[test]
    fn install_core_files_bad_args_non_hex_hash() {
        // 64 字符但非 hex
        let (src, _hash) = make_src_dir();
        let core = tempfile::tempdir().unwrap();
        let result = install_core_files(core.path(), src.path(), &"z".repeat(64));
        assert_eq!(result.unwrap_err(), InstallResult::BadArgs);
    }

    // ===== to_wire_line（锁住 wire 协议，对照 Go 源各 return 分支） =====

    #[test]
    fn to_wire_line_for_all_variants() {
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

    #[test]
    fn is_ok_predicate() {
        assert!(InstallResult::Installed.is_ok());
        assert!(!InstallResult::BadArgs.is_ok());
        assert!(!InstallResult::HashMismatch.is_ok());
    }

    // ===== is_valid_core_dir（移植自 mac 单测） =====

    #[test]
    fn is_valid_core_dir_checks() {
        let parent = tempfile::tempdir().unwrap();
        let core = parent.path().join("core");
        assert!(is_valid_core_dir(&core), "父目录存在 + core 非空 → 有效");
        assert!(!is_valid_core_dir(Path::new("")), "空路径无效");
        assert!(
            !is_valid_core_dir(Path::new("/nonexistent-parent-xyz/core")),
            "父目录不存在 → 无效"
        );
    }
}
