//! SHA256 校验纯逻辑 + tmp→rename 原子替换编排。
//!
//! 移植自：
//!   - `shared/file-hash.ts:sha256File`（createHash('sha256') 整文件哈希）。
//!   - `CoreUpdateService.installCoreFromDir:453-479`（非 Windows staged-rename 落位编排）：
//!     先把全部文件复制到同目录临时名（`.polaris-new-*`，见 `tmp_name`），全部成功后逐个原子 rename 就位；
//!     复制阶段任一失败 → 删临时文件、抛错，现役核心目录完全未动（杜绝「新核心 + 旧/坏 libcronet」半替换混搭）。
//!
//! 设计：校验与 rename 编排都是纯函数（校验吃 `&[u8]` + 期望 hash；编排吃 [`UpdateFs`](crate::UpdateFs) trait
//! 注入），可单测。真实 sha256 算法由 `sha2` crate 提供（与 helper-linux/helper-mac 同口径）。

use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::traits::UpdateFs;

// 去重：复用 polaris-helper-proto 的 is_valid_sha256_hex（旧 verify.rs 本地副本已删）。
// `pub use` 使 `crate::verify::is_valid_sha256_hex`（manifest.rs 调用路径）+ 本模块内调用都解析到 helper-proto 版。
pub use polaris_helper_proto::codec::is_valid_sha256_hex;

/// SHA256 校验错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VerifyError {
    /// 期望的 hash 不是合法的 64 字符 hex（移植自 `helper.go:137` 的 `len(wantHash) != 64` 校验）。
    #[error("invalid expected hash: not 64-char hex (got {0} chars)")]
    InvalidExpectedHash(usize),
    /// 字节流/文件的实际 hash 与期望不符（移植自 `helper.go:146` 的 hash-mismatch）。
    #[error("hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch { expected: String, actual: String },
}

/// 计算字节的 SHA256 hex（小写，移植自 `file-hash.ts:sha256File` 的 `.digest('hex')`）。
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 计算字节的 SHA256 hex（小写，别名——与 helper-linux `sha256_hex` 同名同形，便于跨 crate 对照）。
#[must_use]
pub fn sha256_hex_lower(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

// `is_valid_sha256_hex`（校验 64 字符合法 hex）由本模块顶部的 `pub use` 从
// `polaris_helper_proto::codec` re-export，故 `crate::verify::is_valid_sha256_hex`（manifest.rs 的调用路径）继续可用。
// 用 `//` 而非 `///`：doc 注释必须挂在 item 上，此处后面并无 item（原为悬空 doc → clippy `empty_line_after_doc_comments`）。

/// 校验字节流的 SHA256 是否等于期望（大小写不敏感，对齐 Polaris Go `strings.EqualFold`）。
///
/// 移植自 `CoreUpdateService.installCoreFromDir` 经 helper install-core 路径的 sha256 校验
/// （`helper-linux/core_installer.rs:99` 同款 `eq_ignore_ascii_case`）。
///
/// # Errors
///
/// - [`VerifyError::InvalidExpectedHash`]：`expected_hex` 非 64 字符 hex。
/// - [`VerifyError::HashMismatch`]：实际 hash 与期望不符。
pub fn verify_bytes(bytes: &[u8], expected_hex: &str) -> Result<(), VerifyError> {
    if !is_valid_sha256_hex(expected_hex) {
        return Err(VerifyError::InvalidExpectedHash(expected_hex.len()));
    }
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(VerifyError::HashMismatch {
            expected: expected_hex.to_string(),
            actual,
        })
    }
}

/// 校验 hex（仅格式校验的别名，供 staged 周期在下载前先验 manifest 里的 hash 字段）。
///
/// # Errors
///
/// 仅 [`VerifyError::InvalidExpectedHash`]（不计算任何字节）。
pub fn verify_hex(expected_hex: &str) -> Result<(), VerifyError> {
    if is_valid_sha256_hex(expected_hex) {
        Ok(())
    } else {
        Err(VerifyError::InvalidExpectedHash(expected_hex.len()))
    }
}

/// 单文件原子替换编排：把 `bytes` 写到 `dest` 旁的**唯一**临时名（见 `tmp_name`），再原子 rename 到 `dest`。
///
/// 移植自 `CoreUpdateService.installCoreFromDir:459-469` 的单文件 staged-rename：
///   1. `tmp = tmp_name(dest)`（`{dest}.polaris-new-{pid}-{seq}`，**每次调用都不同**——固定名会让并发替换互相踩，见 `tmp_name`）
///   2. write(tmp, bytes) —— 失败抛出，dest 未动
///   3. rename(tmp → dest) —— 失败删 tmp 残件后抛出（对齐 上游 `if exists then unlink tmp`）
///
/// 同目录 rename 在 Unix/Win 均为原子 syscall（覆盖 dest），故 dest 要么是旧内容、要么是新内容，
/// 不会出现「写到一半」的半替换态。
///
/// # Errors
///
/// 透传 [`UpdateFs::write`] / [`UpdateFs::rename`] 的 IO 错误。rename 失败时尽力删 tmp 残件
/// （残件清理失败被吞，对齐 上游 `try { unlink } catch {}`）。
pub fn atomic_replace(fs: &dyn UpdateFs, dest: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = tmp_name(dest);
    // 1. 写临时文件。失败 → dest 未动；tmp 名带唯一后缀故不会被下次写入覆盖 ⇒ 必须主动删残件。
    if let Err(e) = fs.write(&tmp, bytes) {
        let _ = fs.remove_file(&tmp);
        return Err(e);
    }
    // 2. 原子 rename。失败 → 删 tmp 残件后抛出（防残件累积；清理失败无害）。
    if let Err(e) = fs.rename(&tmp, dest) {
        let _ = fs.remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 多文件原子替换编排（移植自 `CoreUpdateService.installCoreFromDir:457-479` 的 staged-rename 循环）。
///
/// 语义（逐字对齐 Polaris）：
///   1. 先把 `entries`（文件名 → 字节）全部复制到 `dest_dir` 下的唯一临时名（`tmp_name`），
///      记录已复制的 staged rename 列表。
///   2. **复制阶段任一失败** → 删除所有已复制的临时文件、抛错，`dest_dir` 完全未动
///      （杜绝「新核心 + 旧/坏 libcronet」半替换混搭——原逐文件直接覆盖在中途失败时会留下此混搭，
///      而 restoreBackup 仅回滚主二进制救不回）。
///   3. 全部复制成功 → 逐个原子 rename 就位（同目录 rename 几乎不失败）。
///
/// 不变量：要么 `dest_dir` 下所有 `entries` 都换成新内容，要么一个都没换（无中间态）。
///
/// # Errors
///
/// - 复制阶段：透传 [`UpdateFs::write`] 错误（已复制的临时文件会先被清理）。
/// - rename 阶段：透传 [`UpdateFs::rename`] 错误（已 rename 的不回滚——同目录 rename 失败极罕见，
///   且 Polaris 原实现此处也不回滚已就位的，留待上层 restoreBackup 兜底）。
pub fn atomic_replace_multi(
    fs: &dyn UpdateFs,
    dest_dir: &Path,
    entries: &[(String, Vec<u8>)],
) -> Result<(), std::io::Error> {
    // 已复制到 tmp 的（tmp_path, dest_path）对：失败时用于清理。
    let mut staged: Vec<(std::path::PathBuf, std::path::PathBuf)> =
        Vec::with_capacity(entries.len());

    // 1. 全部先复制到临时名。
    for (name, bytes) in entries {
        let dest = fs.join(dest_dir, name);
        let tmp = tmp_name(&dest);
        if let Err(e) = fs.write(&tmp, bytes) {
            // 复制失败：清理已暂存的 tmp 残件，dest_dir 未动。
            for (t, _) in &staged {
                let _ = fs.remove_file(t);
            }
            return Err(e);
        }
        staged.push((tmp, dest));
    }

    // 2. 全部复制成功 → 逐个原子 rename 就位。
    //    rename 失败：Polaris 原实现此处不回滚已就位的（同目录 rename 几乎不失败），透传错误由上层 restoreBackup 兜底。
    for (tmp, dest) in &staged {
        if let Err(e) = fs.rename(tmp, dest) {
            // 清理尚未 rename 的 tmp 残件（已 rename 的不回滚）。
            let _ = fs.remove_file(tmp);
            return Err(e);
        }
    }
    Ok(())
}

/// 生成**每次调用都不同**的临时名：`{dest}.polaris-new-{pid}-{seq}`。
///
/// # 为什么不能沿用固定的 `.polaris-new`
///
/// 固定名让「同一个 `dest` 的两次并发原子替换」互相踩：A 的 `write` 还没写完，B 的 `write`
/// 以 truncate 打开**同一个** tmp 从头覆盖 → 随后任一方的 `rename` 都可能把一个长度不对的
/// 半截文件搬成 `dest`。而先返回的那一方已经报了「成功 + 已校验」——校验对象是内存里的字节，
/// **不是**落盘后的文件，故这种破损完全不会被现有校验拦住（安装时才炸）。
///
/// 并发不是异常路径：`autoDownloadUpdate` 开启时，启动腿在后台下载的同时弹 remind 窗邀请用户
/// 点「更新」，两条腿写的正是同一个 dest。加唯一后缀后，两条腿各写各的 tmp，`dest` 恒是
/// 某一方的**完整**内容。
///
/// 上游 原名是 `${dest}.polaris-new`（`CoreUpdateService.ts:462`）；此处刻意分叉并留档。
///
/// **已知代价（如实登记）**：进程在 write 与 rename 之间被硬杀会留下带唯一后缀的残件
/// （固定名那版会被下一次写入覆盖掉）。两条正常失败路径（write / rename 报错）都会主动删，
/// 故只在硬崩时残留；换核的暂存目录每次 `stage` 都整目录重建，也会一并清掉。
fn tmp_name(dest: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    /// 进程内单调序号：同一毫秒内的多次调用也不会撞名。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut s = dest.as_os_str().to_os_string();
    s.push(format!(".polaris-new-{}-{seq}", std::process::id()));
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::StdFs;

    // 已知内容的标准 SHA256（用 echo -n | sha256sum 验证）：
    //   sha256(b"")     = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    //   sha256(b"hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA);
        assert_eq!(sha256_hex(b"hello"), HELLO_SHA);
        // 大小写：hex::encode 恒输出小写。
        assert_eq!(sha256_hex(b"hello"), sha256_hex_lower(b"hello"));
        assert!(sha256_hex(b"hello")
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn is_valid_sha256_hex_variants() {
        assert!(is_valid_sha256_hex(&"a".repeat(64)));
        assert!(is_valid_sha256_hex(&"ABCDEF0123456789".repeat(4))); // 大小写混用
        assert!(!is_valid_sha256_hex("abc")); // 太短
        assert!(!is_valid_sha256_hex(&"z".repeat(64))); // 非 hex
        assert!(!is_valid_sha256_hex(&"a".repeat(63))); // 63 字符
    }

    #[test]
    fn verify_bytes_match_case_insensitive() {
        // 匹配（小写期望）。
        assert!(verify_bytes(b"hello", HELLO_SHA).is_ok());
        // 匹配（大写期望——对齐 Polaris strings.EqualFold 大小写不敏感）。
        assert!(verify_bytes(b"hello", &HELLO_SHA.to_uppercase()).is_ok());
        // 不匹配。
        let err = verify_bytes(b"hello", EMPTY_SHA).unwrap_err();
        assert!(matches!(err, VerifyError::HashMismatch { .. }));
    }

    #[test]
    fn verify_bytes_invalid_expected() {
        // 非 64 字符 hex。
        let err = verify_bytes(b"hello", "abc").unwrap_err();
        assert_eq!(err, VerifyError::InvalidExpectedHash(3));
        // 非 hex（64 字符但含 z）。
        let err = verify_bytes(b"hello", &"z".repeat(64)).unwrap_err();
        assert_eq!(err, VerifyError::InvalidExpectedHash(64));
    }

    #[test]
    fn verify_hex_only_format() {
        assert!(verify_hex(HELLO_SHA).is_ok());
        assert_eq!(
            verify_hex("abc").unwrap_err(),
            VerifyError::InvalidExpectedHash(3)
        );
    }

    #[test]
    fn atomic_replace_single() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let dest = fs.join(tmpdir.path(), "core.bin");

        // 首次替换：dest 不存在 → 写 tmp → rename 成功。
        atomic_replace(&fs, &dest, b"new-content").unwrap();
        assert_eq!(fs.read(&dest).unwrap(), b"new-content");
        // tmp 不残留（tmp 名带唯一后缀，故按「目录里只剩 dest」断言，而不是猜某个具体 tmp 名）。
        assert_eq!(
            fs.list_files(tmpdir.path()).unwrap(),
            vec!["core.bin".to_string()]
        );

        // 二次替换：dest 已存在 → 原子覆盖。
        atomic_replace(&fs, &dest, b"v2").unwrap();
        assert_eq!(fs.read(&dest).unwrap(), b"v2");
        assert_eq!(
            fs.list_files(tmpdir.path()).unwrap(),
            vec!["core.bin".to_string()]
        );
    }

    /// 🟡 **变异锁：tmp 名必须每次调用都不同。**
    ///
    /// 把 [`tmp_name`] 改回固定的 `{dest}.polaris-new` ⇒ 本条转红。它是
    /// [`concurrent_atomic_replace_never_yields_a_torn_dest`] 的判据来源：并发写同一个 tmp 才是撕裂的成因。
    #[test]
    fn tmp_name_is_unique_per_call() {
        let dest = Path::new("/tmp/whatever/core.bin");
        let a = tmp_name(dest);
        let b = tmp_name(dest);
        assert_ne!(
            a, b,
            "固定 tmp 名 ⇒ 并发原子替换会把半截文件 rename 成 dest"
        );
        for p in [&a, &b] {
            let s = p.to_string_lossy().into_owned();
            assert!(
                s.starts_with("/tmp/whatever/core.bin.polaris-new-"),
                "tmp 必须与 dest **同目录**（跨目录 rename 不是原子操作），实得 {s}"
            );
        }
    }

    /// 🟡 **并发原子替换绝不产生撕裂的 dest。**
    ///
    /// 复现的是生产里的默认形态：`autoDownloadUpdate` 的后台下载腿与用户在 mini 弹窗点「更新」
    /// 触发的下载腿写**同一个** dest。用固定 tmp 名时，两条腿的 write(truncate) 与 rename 交错
    /// 会把长度不对的半截文件搬成 dest（且先完成方已报 success/verified）。
    ///
    /// **变异探针**：把 `tmp_name` 改回固定名 ⇒ 读侧断言（dest 内容必须是某一方的完整载荷）转红。
    #[test]
    fn concurrent_atomic_replace_never_yields_a_torn_dest() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dest = StdFs.join(tmpdir.path(), "update.pkg");
        // 三份**长度各异**的载荷：撕裂一定表现为「长度对不上任何一份」或「内容混杂」。
        let payloads: Vec<Vec<u8>> = (0..3u8)
            .map(|i| vec![b'a' + i; 4096 * (usize::from(i) + 1)])
            .collect();
        // 先落一份合法内容，读侧从第一拍起就有东西可读。
        atomic_replace(&StdFs, &dest, &payloads[0]).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut writers = Vec::new();
        for p in payloads.clone() {
            let dest = dest.clone();
            writers.push(std::thread::spawn(move || {
                for _ in 0..60 {
                    atomic_replace(&StdFs, &dest, &p).unwrap();
                }
            }));
        }
        let reader = {
            let (dest, stop, payloads) = (dest.clone(), stop.clone(), payloads.clone());
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(got) = std::fs::read(&dest) {
                        assert!(
                            payloads.contains(&got),
                            "dest 出现了撕裂内容（长度 {}）—— 并发原子替换把半截文件 rename 成了 dest",
                            got.len()
                        );
                        reads += 1;
                    }
                }
                reads
            })
        };
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0, "读侧一次都没读到 → 断言没生效");
        // 收尾：只剩 dest，无 tmp 残件。
        assert_eq!(
            StdFs.list_files(tmpdir.path()).unwrap(),
            vec!["update.pkg".to_string()]
        );
    }

    #[test]
    fn atomic_replace_multi_all_or_nothing() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let dest_dir = fs.join(tmpdir.path(), "target");
        fs.create_dir_all(&dest_dir).unwrap();

        let entries = vec![
            ("sing-box".to_string(), b"bin-content".to_vec()),
            ("libcronet.so".to_string(), b"lib-content".to_vec()),
        ];
        atomic_replace_multi(&fs, &dest_dir, &entries).unwrap();

        // 两个文件都就位，无 tmp 残件。
        assert_eq!(
            fs.read(&fs.join(&dest_dir, "sing-box")).unwrap(),
            b"bin-content"
        );
        assert_eq!(
            fs.read(&fs.join(&dest_dir, "libcronet.so")).unwrap(),
            b"lib-content"
        );
        let files = fs.list_files(&dest_dir).unwrap();
        assert_eq!(
            files,
            vec!["libcronet.so".to_string(), "sing-box".to_string()]
        );
    }

    #[test]
    fn atomic_replace_multi_overwrites_existing() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let dest_dir = fs.join(tmpdir.path(), "target");
        fs.create_dir_all(&dest_dir).unwrap();
        // 预置旧文件。
        fs.write(&fs.join(&dest_dir, "sing-box"), b"old").unwrap();

        let entries = vec![("sing-box".to_string(), b"new".to_vec())];
        atomic_replace_multi(&fs, &dest_dir, &entries).unwrap();
        assert_eq!(fs.read(&fs.join(&dest_dir, "sing-box")).unwrap(), b"new");
    }
}
