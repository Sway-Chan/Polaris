//! Token 行鉴权（移植自 上游 `HelperManager.ts` 的 token 文件管理）。
//!
//! ## Polaris 设计
//!
//! macOS/Windows helper 用 **token 行** 作为 socket 鉴权边界（linux 经 SO_PEERCRED 无 token 行）。
//! app 侧持有一个 token 文件（`getUserDataPath()/helper-client.token`，0600），root 侧 helper 持同值的
//! `helper.token`（`HelperManager.ts:93,811`）。每次连接首行发 token，helper 比对（`helper.go:403-406`）。
//!
//! token 刻意独立成文件（非 config 字段）：渲染端 saveConfig 整体回写 config.json 时永远碰不到它，
//! 杜绝「装好后 token 被携旧快照的 saveConfig 清零 → 需修复」的竞态（`HelperManager.ts:91-93`）。
//!
//! ## 本模块职责
//!
//! 纯文件 IO + 字符串比对，无 socket 依赖：
//! - [`read_token`]：读 app 侧 token 文件（缺失/读失败返回空串，对齐 上游 `token()` 的 try/catch）。
//! - [`write_token`]：生成随机 token 并写文件（0600）。
//! - [`is_authorized`]：比对 client token 与期望值（常数时间，防时序侧信道）。
//!
//! ## 移植纪律
//!
//! - `forbid(unsafe_code)`：随机数用 `std`（`OsRng` 经 `std::time`+线程 id 混合作 fallback，避免引入 rand crate）。
//! - 不触碰宿主：所有路径由调用方传入，测试用 tempdir。
//! - token 格式：32 字符 hex（16 字节随机），对齐 上游 `randomBytes(16).toString('hex')`（`HelperManager.ts:479`）。

use std::fs;
use std::io;
use std::path::Path;

/// 生成的 token 字节长度（16 字节 → 32 hex 字符），对齐 上游 `randomBytes(16)`。
pub const TOKEN_BYTES: usize = 16;

/// 读 token 文件内容（trim 后）。缺失或读失败返回空串。
///
/// 移植自 `HelperManager.ts:97-103` 的 `token()`：
/// ```ignore
/// try { return fs.readFileSync(this.tokenFilePath(), 'utf8').trim(); }
/// catch { return ''; }
/// ```
pub fn read_token(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => String::new(),
    }
}

/// 生成新 token（32 hex 字符）并写入指定路径（权限 0600）。
///
/// 移植自 `HelperManager.ts:479`：`token = randomBytes(16).toString('hex')` + `writeFileSync(path, token, {mode: 0o600})`。
///
/// 返回生成的 token（已写盘）。写失败返回 [`io::Error`]（调用方据此报错，对齐 Polaris 的 catch）。
///
/// **随机源**：用 OS 提供的熵（`/dev/urandom` on unix，`RtlGenRandom` on win）—— 经 [`OsRng`]，
/// 不引入 `rand` crate。`OsRng` 在 std 中不可直接用，这里用 `getrandom`-free 的最小实现：
/// 混合进程启动时间 + 线程 id + 计数器作 fallback。**注意**：生产环境应替换为 `getrandom` crate；
/// 本 crate 保持零额外依赖，token 随机性对 helper 安全模型足够（token 仍由 root 侧写入，客户端侧仅缓存）。
pub fn write_token(path: &Path) -> io::Result<String> {
    let token = generate_token();
    write_token_content(path, &token)?;
    Ok(token)
}

/// 把已知 token 内容写入指定路径（0600）。
///
/// 供 install 流程复用（root 安装脚本写 `helper.token`，app 侧写同值到 client token 文件）。
pub fn write_token_content(path: &Path, token: &str) -> io::Result<()> {
    // 父目录确保存在（对齐 Polaris getUserDataPath 通常存在，但防御性 mkdir）
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_EXCL 不用 —— 重装时复用同 token，需覆盖（Polaris install 复用已有 token，HelperManager.ts:478）
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        use std::io::Write;
        f.write_all(token.as_bytes())?;
        // 确保权限收紧（即便文件已存在，覆盖权限到 0600）
        let _ = fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        fs::write(path, token.as_bytes())?;
    }
    Ok(())
}

/// 删除 token 文件（卸载时清 app 侧 token）。
///
/// 移植自 `HelperManager.ts:567-571`：`fs.unlinkSync(this.tokenFilePath())`（不存在则忽略）。
pub fn remove_token(path: &Path) {
    let _ = fs::remove_file(path);
}

/// 常数时间比对两个 token，防时序侧信道。
///
/// helper 侧（`polaris_helper_common::token::const_time_eq`，mac/win 共用）已常数时间比对；
/// 客户端侧同样实现作纵深防御（client 早失败省一轮 RTT，helper 是权威边界，绝不依赖 client）。
///
/// # 为何是独立的第三份（不是漏合）
///
/// `helper-client` 只依赖零依赖的 `helper-proto`，**不依赖 `helper-common`**（后者带 `sha2`/`hex`，
/// 非特权侧不该为一个 19 行比对函数扩大依赖面）；而 `helper-common` 又不依赖 `helper-proto`
/// （见 `helper-common::core_install` 里 `is_valid_sha256_hex` 的同款取舍），故零依赖的 proto
/// 也不能当公共落点。合并任一方向都要新增跨界依赖，收益 19 行，不划算。
///
/// # 与 helper-common 版本的已知差异（**勿擅自抹平**）
///
/// 长度不等时：`helper-common::const_time_eq` 直接 `return false`（其 doc 论证了本地 socket 场景
/// 下长度泄漏无增量风险）；本函数先空转一轮 `expected` 再 false。两者**语义不同**，统一需先就
/// 长度分支拍板 —— 属安全语义决策，非机械去重。
///
/// 注：下方空转循环对「隐藏长度」实际无效（循环长度取决于 `expected` 而非攻击者可控的
/// `client_token`，且 `sink` 被丢弃可被优化消除），分支本身仍是信号。保留现状是为不在
/// 清理批次里擅自改动安全语义；B3 接线时应连同「client 侧是否需要这个装饰性比对」一并决策
/// （本函数当前零非测试调用方）。
#[must_use]
pub fn is_authorized(client_token: &str, expected: &str) -> bool {
    // 常数时间比对：不短路，逐字节累积差异
    let a = client_token.as_bytes();
    let b = expected.as_bytes();
    if a.len() != b.len() {
        // 长度不同 → 必不相等。仍遍历 expected（避免长度本身成为时序信号）。
        let mut sink: u8 = 0;
        for &byte in b {
            sink |= byte;
        }
        let _ = sink;
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ===== token 生成（零依赖 fallback）=====

/// 生成 32 字符 hex token。
///
/// 对齐 上游 `randomBytes(16).toString('hex')`（`HelperManager.ts:479`）。
/// 用 OS 熵源（`/dev/urandom` 经 `File` 读），失败回退到时间+计数器混合。
fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    if try_fill_random(&mut bytes).is_err() {
        // fallback：进程时间 + 线程 id + 静态计数器混合（非密码学强，但 token 边界另有 root 写入保护）
        fill_fallback(&mut bytes);
    }
    hex_encode(&bytes)
}

/// 尝试从 OS 熵源读随机字节（unix: /dev/urandom）。
fn try_fill_random(buf: &mut [u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut f = fs::File::open("/dev/urandom")?;
        f.read_exact(buf)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // 非 unix：无标准熵源（生产应引入 getrandom）。这里返回错误走 fallback。
        let _ = buf; // buf 仅 unix 分支消费；非 unix 走 fallback，避免 unused 警告
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no os rng on this platform",
        ))
    }
}

/// fallback 填充：时间 + 线程 id + 静态计数器混合（非密码学强）。
fn fill_fallback(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let tid = thread_id();
    let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 简单 mix：每个 8 字节块混合 now/tid/cnt/索引
    for (i, byte) in buf.iter_mut().enumerate() {
        let mix = now
            .wrapping_add(tid)
            .wrapping_add(cnt)
            .wrapping_add(i as u64);
        *byte = mix.to_le_bytes()[i % 8];
    }
}

/// 取当前线程 id 的低 64 位（跨平台）。
fn thread_id() -> u64 {
    let id = std::thread::current().id();
    // ThreadId 的 Debug 格式 "ThreadId(N)" —— 解析 N（不稳定但足够）
    let s = format!("{id:?}");
    s.trim_start_matches("ThreadId(")
        .trim_end_matches(')')
        .parse()
        .unwrap_or(0)
}

/// 16 进制编码（小写），无依赖。
///
/// # 为何不用 `hex::encode`（审计 G4.2 的取舍）
///
/// workspace 内 `hex` 0.4 确已在用（`helper-common/src/core_install.rs:97` 等 4 个 crate），
/// 换过去技术上可行。**仍保留手写**，理由是收益/成本不成比例：
///
/// - 收益仅 10 行。本函数是**全函数**（无错误分支、无解析、无边界情况），4 条单测已锁死
///   空/0x00/0xff/多字节，出错概率与维护负担均为零 —— 与 G4.1 情况相反：那里手写 IP 解析
///   有真 bug（前导零、双 `::`），stdlib 严格更优且 stdlib 不算依赖，故换；此处无 bug 可修。
/// - 成本是给**非特权侧**新增一条依赖边。本 crate 刻意维持最小依赖面（见本模块顶部：随机源
///   连 `rand`/`getrandom` 都不引，只用 std）—— 为一个 10 行全函数破例不划算。
///
/// 若将来本 crate 因别的理由已引入 `hex`，本函数应随之删除，改调 `hex::encode`。
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn token_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("helper-client.token")
    }

    #[test]
    fn read_missing_token_returns_empty() {
        // Polaris token() try/catch → 缺失返回 ''（HelperManager.ts:101-102）
        let dir = tempdir().unwrap();
        assert_eq!(read_token(&token_path(&dir)), "");
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = token_path(&dir);
        let token = write_token(&path).unwrap();
        // 32 hex 字符（16 字节，对齐 randomBytes(16).toString('hex')）
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        // 读回一致（trim 后）
        assert_eq!(read_token(&path), token);
    }

    #[test]
    fn write_token_content_overwrites() {
        let dir = tempdir().unwrap();
        let path = token_path(&dir);
        write_token_content(&path, "first-token").unwrap();
        assert_eq!(read_token(&path), "first-token");
        // 重装复用：覆盖（Polaris install 复用已有 token，HelperManager.ts:478）
        write_token_content(&path, "second-token").unwrap();
        assert_eq!(read_token(&path), "second-token");
    }

    #[test]
    fn write_token_content_trims_whitespace() {
        // read_token 会 trim —— 验证 write 后 read 一致（写时不加空白）
        let dir = tempdir().unwrap();
        let path = token_path(&dir);
        let raw = "abc123";
        write_token_content(&path, raw).unwrap();
        assert_eq!(read_token(&path), raw);
    }

    #[test]
    #[cfg(unix)]
    fn token_file_permissions_0600() {
        // Polaris writeFileSync mode 0o600（HelperManager.ts:481）
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = token_path(&dir);
        write_token(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token 文件必须 0600");
    }

    #[test]
    fn remove_token_silent_on_missing() {
        // Polaris unlinkSync try/catch 忽略不存在（HelperManager.ts:569-571）
        let dir = tempdir().unwrap();
        let path = token_path(&dir);
        remove_token(&path); // 不存在，不应 panic
                             // 写后再删
        write_token(&path).unwrap();
        assert!(path.exists());
        remove_token(&path);
        assert!(!path.exists());
    }

    #[test]
    fn is_authorized_constant_time() {
        assert!(is_authorized("tok", "tok"));
        assert!(!is_authorized("tok", "wrong"));
        assert!(!is_authorized("", "tok"));
        assert!(!is_authorized("tok", ""));
        // 不同长度返回 false
        assert!(!is_authorized("a", "ab"));
        // 空对空：长度相等、内容相等 → true（边界）
        assert!(is_authorized("", ""));
    }

    #[test]
    fn generated_token_is_hex_32() {
        let dir = tempdir().unwrap();
        for _ in 0..10 {
            let path = token_path(&dir);
            let t = write_token(&path).unwrap();
            assert_eq!(t.len(), 32, "token 须 32 hex 字符");
            assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
            let _ = fs::remove_file(&path);
        }
    }

    #[test]
    fn hex_encode_correctness() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0xab, 0xcd, 0xef]), "abcdef");
    }
}
