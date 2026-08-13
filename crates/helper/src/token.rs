//! token 行鉴权（合并自 `helper-mac/src/token.rs` + `helper-win/src/token.rs`，移植自 Polaris
//! `helper/helper.go:87-90,403-408` 与 `helper-win/helper.go:74-77,169`）。
//!
//! ## 为何合并
//!
//! mac/win 两份 `token.rs` 的 `TOKEN_FILENAME` / [`TokenStore`] trait / [`FileTokenStore`] /
//! [`StaticTokenStore`] 逐字等价（架构穷举报告确认）。本模块抽公共、**特权侧（mac/win helper）
//! 的单一真值**；差异（mac 自实现常量时间比对 vs win 用 `==`）统一到**常量时间比对**——mac 的
//! 加固版推广到 win，零成本安全升级。
//!
//! ## 合并范围的诚实边界（勿再误读为「全仓单一真值」）
//!
//! 本模块**只覆盖特权 helper 侧**。非特权侧的 `helper-client/src/token.rs::is_authorized` 是
//! **第三个独立实现**，未合并 —— 原因是依赖图，不是疏漏：
//!
//! - `helper-client` 不依赖 `helper-common`（本 crate 带 `sha2`/`hex`，非特权侧不该为一个
//!   19 行比对函数牵连进来）。
//! - `helper-common` 不依赖 `helper-proto`（见 [`crate::core_install`] 里 `is_valid_sha256_hex`
//!   同款取舍）—— 故也无法借零依赖的 proto 作公共落点。
//!
//! 两份**并非逐字同构**：本模块长度不等直接 `return false`（理由见 [`const_time_eq`]）；
//! client 侧长度不等时先空转一轮 `expected` 再 false。合并需先就长度分支语义拍板，
//! 属安全语义决策，不在「机械去重」范围内 —— 留给 B3 接线时连同「client 侧是否需要这个
//! 装饰性比对」一并定（该函数当前零非测试调用方）。
//!
//! ## 威胁模型（逐字对照 Go 源 `helper.go:4-9` / `helper-win/helper.go:8-13`）
//!
//! token 是 helper 的**主鉴权边界**：
//! - macOS：helper 监听 0666 unix socket，任何本地进程都能连，token 是唯一安全边界（socket 权限只防远程）。
//! - Windows：命名管道虽有 SDDL ACL（纵深防御），但 token 仍是主边界。
//!
//! token 文件 `helper.token` 仅特权账户可读（mac 600/root，win SYSTEM）；app 持自身副本经 socket/管道
//! 第 1 行发送，helper 读自身文件比对。残余风险：能读到 app 配置目录内 token 的同用户进程可驱动 helper
//!（Polaris 未签名 → 无法做 SMJobBless 客户端证书校验）。token + 二进制路径锁定 + 配置目录约束为
//! 现实可行的缓解组合。
//!
//! ## 协议形态（`helper.go:403-404` / `helper-win/helper.go:167-168`）
//!
//! ```text
//! 行1: <token>   ← 客户端发送的副本
//! 行2: <command>
//! ```
//! helper 读行1 与 [`TokenStore::token_value`] 比对，不等立即 `ERR auth`。
//!
//! ## 移植纪律
//!
//! Go `tokenValue()` 读 `supportDir/helper.token` 全文 trim。本模块把「读 token 文件」抽象为
//! [`TokenStore`] trait —— 测试可注入内存副本（不触碰宿主真 token 文件），生产 impl 读真实路径。
//! 鉴权判定本身（[`check_token`]）是常量时间比对，纯逻辑、跨平台可测。
//!
//! ## 常量时间比对
//!
//! [`const_time_eq`] 自实现、不依赖 `subtle` 等 crate：token 长度短且固定，纯算术异或累计足够。
//! helper-common 是 `#![forbid(unsafe_code)]`——本函数纯算术、无 unsafe。

use std::path::{Path, PathBuf};

/// token 文件名（`helper.go:88` / `helper-win/helper.go:75`：`filepath.Join(supportDir, "helper.token")`
/// 的末段）。常量单一真值，跨平台逐字同。
pub const TOKEN_FILENAME: &str = "helper.token";

/// token 存储抽象 —— 生产读文件，测试注入内存值。
///
/// 对应 Go `tokenValue()` 的「读 supportDir/helper.token 并 trim」语义。trait 化是为测试可注入
/// （不触碰宿主真 token 文件，本机 Linux CI 也不需 root）。
pub trait TokenStore: Send + Sync {
    /// 返回当前 token 值（已 trim）。读失败 / 文件不存在返回空串（等价 Go `b, _ := os.ReadFile(...)`
    /// 忽略 err）。
    fn token_value(&self) -> String;
}

/// 文件系统 token 存储生产实现（对应 Go `os.ReadFile(filepath.Join(supportDir, "helper.token"))`）。
///
/// 构造时存 `support_dir`，读取时 join `helper.token`（Go `filepath.Join` 语义）。
/// trim 后返回；失败 / 不存在 → 空串（Go `_` 吞错误同款）。任何非空客户端 token 与空存储比对 →
/// [`TokenCheck::EmptyStored`] → 永远拒，安全失败。
///
/// 注意：本类型在所有平台编译（读文件本身跨平台），生产侧由各 helper 在各自平台实例化
///（mac LaunchDaemon 路径 / win SCM 路径）。
#[derive(Debug, Clone)]
pub struct FileTokenStore {
    support_dir: PathBuf,
}

impl FileTokenStore {
    /// 构造：`support_dir` 即 Go `--support` flag 值（mac 默认
    /// `/Library/Application Support/Polaris`，win 默认 `C:\ProgramData\Polaris`）。token 文件路径
    /// 延迟 join（[`token_path`]），与 Go `filepath.Join(supportDir, "helper.token")` 一致。
    #[must_use]
    pub fn new(support_dir: impl AsRef<Path>) -> Self {
        Self {
            support_dir: support_dir.as_ref().to_path_buf(),
        }
    }

    /// token 文件完整路径（`support_dir/helper.token`，诊断 / 日志 / 安装期写入用）。
    #[must_use]
    pub fn token_path(&self) -> PathBuf {
        self.support_dir.join(TOKEN_FILENAME)
    }

    /// token 文件完整路径的引用视图（win 既有 `path()` 习惯的兼容别名，等价
    /// `self.token_path()` 但免 alloc）。
    ///
    /// 注意：本方法借用 `&self`，返回 `PathBuf` 的 clone 之外的零拷贝路径需经 [`token_path`]。
    /// 为兼容 win 旧调用点（`store.path()`），此处返回完整路径。
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.token_path()
    }
}

impl TokenStore for FileTokenStore {
    fn token_value(&self) -> String {
        // Go: b, _ := os.ReadFile(...); return strings.TrimSpace(string(b))
        match std::fs::read(self.token_path()) {
            Ok(b) => String::from_utf8_lossy(&b).trim().to_owned(),
            Err(_) => String::new(),
        }
    }
}

/// 静态 token 存储桩（测试用内存副本，移植自 `helper-win` 的同名类型）。
#[derive(Debug, Clone)]
pub struct StaticTokenStore {
    token: String,
}

impl StaticTokenStore {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl TokenStore for StaticTokenStore {
    fn token_value(&self) -> String {
        self.token.clone()
    }
}

/// token 比对结果（移植自 `helper-mac` 的 `TokenCheck`，扩展为四分支以提供更细粒度的失败诊断）。
///
/// 严格对照 Go `helper.go:405-407` / `helper-win/helper.go:169`：
/// `if tok == "" || tok != tokenValue() { ERR auth }`。两条件任一成立即拒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCheck {
    /// token 匹配，继续处理命令。
    Authed,
    /// 客户端发空行 → 拒（防无 token 进程连上即通过耗资源）。
    EmptyClient,
    /// 服务端无 token（文件缺失 / 读失败 / 空文件）→ 拒（安装期未就绪的安全失败）。
    EmptyStored,
    /// 两端非空但不等 → 拒（token 不匹配）。
    Mismatch,
}

impl TokenCheck {
    /// 是否鉴权通过（便利判定，等价 `matches!(self, TokenCheck::Authed)`）。
    #[must_use]
    pub fn is_authed(self) -> bool {
        matches!(self, Self::Authed)
    }
}

/// 比对客户端发送的 token 行与存储值，返回细粒度结果（移植自 `helper-mac` 的 `verify_token`）。
///
/// 双重拒空 + 常量时间比对：
/// - 客户端空 → [`TokenCheck::EmptyClient`]。
/// - 存储空 → [`TokenCheck::EmptyStored`]。
/// - 两端非空 → [`const_time_eq`] 比对，匹配 → [`TokenCheck::Authed`]，否则 [`TokenCheck::Mismatch`]。
pub fn check_token(client_token: &str, stored_token: &str) -> TokenCheck {
    if client_token.is_empty() {
        return TokenCheck::EmptyClient;
    }
    if stored_token.is_empty() {
        return TokenCheck::EmptyStored;
    }
    if const_time_eq(client_token.as_bytes(), stored_token.as_bytes()) {
        TokenCheck::Authed
    } else {
        TokenCheck::Mismatch
    }
}

/// 鉴权判定（bool 版便利函数，取代 `helper-win` 旧 `is_authed` 的 `==` 版本，**升级为常量时间比对**）。
///
/// 双重拒空：客户端 token 为空 → 拒；服务端无 token → 拒。两端非空走 [`const_time_eq`]。
///
/// # 参数
///
/// - `client_tok`：客户端发来的 token 行（已 trim `\r\n`，对应 Go `readLine`）。
/// - `expected`：服务端真值（来自 [`TokenStore::token_value`]）。
///
/// # 返回
///
/// `true` = 鉴权通过；`false` = 拒（helper 应回 `ERR auth`）。
///
/// # 常量时间比对
///
/// 本实现**升级**为常量时间比对（[`const_time_eq`]）——原 `helper-win` 用 `==` 非常量时间。token 不是
/// 密码而是「持有者即授权」凭据，非常数时间比对的增量风险低，但常量时间无额外成本（token 长度短且固定），
/// 故统一采用作为纵深防御。等价 [`check_token`] 返回 [`TokenCheck::Authed`]。
#[must_use]
pub fn is_authed_constant_time(client_tok: &str, expected: &str) -> bool {
    check_token(client_tok, expected).is_authed()
}

/// 常量时间字节比对（防计时侧信道，移植自 `helper-mac` 自实现，下沉到公共层）。
///
/// 不依赖第三方 crate（`subtle` 等）—— token 长度短且固定，纯算术自实现足够。算法：长度不等直接返回
/// false（本地 socket 场景攻击者已知自己发的长度，长度泄漏无增量风险），等长则累计所有字节异或，
/// 全零才相等。本函数防的是字节值差导致的提前退出计时差（朴素 `==` 短路逻辑）。
///
/// helper-common 是 `#![forbid(unsafe_code)]`——本函数纯算术、零 unsafe。
pub fn const_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 便利：判断给定 support_dir 下 token 文件是否存在（安装期检查用，移植自 `helper-mac`）。
#[must_use]
pub fn token_file_exists(support_dir: &Path) -> bool {
    support_dir.join(TOKEN_FILENAME).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存 TokenStore，验证 trait object 注入（不触碰宿主文件系统）。
    #[test]
    fn trait_object_works_via_static_store() {
        let store: Box<dyn TokenStore> = Box::new(StaticTokenStore::new("in-mem-tok"));
        assert_eq!(store.token_value(), "in-mem-tok");
    }

    // --- const_time_eq 行为（移植自 helper-mac）---

    #[test]
    fn const_time_eq_basic() {
        assert!(const_time_eq(b"abc", b"abc"));
        assert!(!const_time_eq(b"abc", b"abd"));
        assert!(!const_time_eq(b"abc", b"ab"));
        assert!(const_time_eq(b"", b""));
        assert!(!const_time_eq(b"", b"a"));
        assert!(!const_time_eq(b"a", b""));
        // 字节值差（非前缀差）：朴素 == 在第一差字节短路，常量时间全遍历
        assert!(!const_time_eq(b"abcdef", b"abcxef"));
        assert!(const_time_eq(b"abcdef", b"abcdef"));
    }

    // --- check_token 各分支（移植自 helper-mac verify_token + 扩展四分支）---

    #[test]
    fn check_matching_token_authed() {
        // helper.go:405: tok == tokenValue() → 通过
        assert_eq!(check_token("sekret", "sekret"), TokenCheck::Authed);
        assert!(check_token("sekret", "sekret").is_authed());
    }

    #[test]
    fn check_empty_client_token() {
        // helper.go:405: tok == "" → ERR auth（防无 token 进程连上不发数据耗资源）
        assert_eq!(check_token("", "sekret"), TokenCheck::EmptyClient);
        assert!(!check_token("", "sekret").is_authed());
    }

    #[test]
    fn check_empty_stored_token() {
        // 存储端为空（token 文件缺失/读失败）：任何非空客户端 token 都不匹配 → 安全失败
        assert_eq!(check_token("anything", ""), TokenCheck::EmptyStored);
        assert!(!check_token("anything", "").is_authed());
    }

    #[test]
    fn check_mismatched_token() {
        // helper.go:405: tok != tokenValue() → ERR auth
        assert_eq!(check_token("wrong", "sekret"), TokenCheck::Mismatch);
        assert!(!check_token("wrong", "sekret").is_authed());
    }

    #[test]
    fn check_both_empty_returns_empty_client() {
        // 双空：先命中客户端空分支（短路顺序与 Go `tok == "" || ...` 一致）
        assert_eq!(check_token("", ""), TokenCheck::EmptyClient);
    }

    // --- is_authed_constant_time（win 升级版，行为等价旧 is_authed 但走常量时间）---

    #[test]
    fn is_authed_constant_time_matches_go_logic() {
        // Go: tok == "" || tok != tokenValue() → false；本函数返回相反（true=通过）
        let expected = "real-token-abc";
        assert!(is_authed_constant_time("real-token-abc", expected)); // 匹配
        assert!(!is_authed_constant_time("wrong", expected)); // 不匹配
        assert!(!is_authed_constant_time("", expected)); // 客户端空 → 拒
        assert!(!is_authed_constant_time("real-token-abc", "")); // 服务端无 token → 拒
        assert!(!is_authed_constant_time("", "")); // 双空 → 拒
    }

    // --- FileTokenStore 读取 + trim（跨平台，Linux 可跑）---

    #[test]
    fn file_token_store_reads_and_trims() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(dir.path());
        // 文件不存在 → 空串
        assert_eq!(store.token_value(), "");
        // 写入带空白的 token
        std::fs::write(store.token_path(), "  my-token-123\n").unwrap();
        // trim 后返回
        assert_eq!(store.token_value(), "my-token-123");
    }

    #[test]
    fn file_token_store_returns_empty_on_missing_file() {
        let s = FileTokenStore::new("/nonexistent/polaris-test-dir-xyz");
        assert_eq!(s.token_value(), "");
    }

    #[test]
    fn file_token_store_token_path_joins_filename() {
        let store = FileTokenStore::new("/Library/Application Support/Polaris");
        assert_eq!(
            store.token_path(),
            PathBuf::from("/Library/Application Support/Polaris/helper.token")
        );
    }

    #[test]
    fn file_token_store_path_alias_matches_token_path() {
        // win 既有 path() 习惯兼容别名，应与 token_path() 逐字等价
        let store = FileTokenStore::new(r"C:\ProgramData\Polaris");
        assert_eq!(store.path(), store.token_path());
    }

    #[test]
    fn file_token_store_path_uses_platform_separator() {
        // win 旧测试断言：PathBuf::join 用各自平台分隔符
        let s = FileTokenStore::new(r"C:\ProgramData\Polaris");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            s.path().to_string_lossy(),
            format!("C:\\ProgramData\\Polaris{sep}helper.token")
        );
    }

    // --- StaticTokenStore（移植自 helper-win）---

    #[test]
    fn static_store_returns_token() {
        let s = StaticTokenStore::new("tok-xyz");
        assert_eq!(s.token_value(), "tok-xyz");
    }

    // --- token_file_exists（移植自 helper-mac）---

    #[test]
    fn token_file_exists_check() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!token_file_exists(dir.path()));
        std::fs::write(dir.path().join(TOKEN_FILENAME), "x").unwrap();
        assert!(token_file_exists(dir.path()));
    }
}
