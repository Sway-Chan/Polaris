//! SO_PEERCRED 鉴权 + 授权 uid 列表（移植自 上游 `helper-linux/helper.go:77-115`）。
//!
//! Linux 无 token 行：对端进程身份经内核 `SO_PEERCRED` 取得（uid/gid 在 connect 时锁定、不可伪造），
//! 再查 root-owned authfile 的 uid 允许列表。
//!
//! ## 移植纪律
//! - Go 源 `peerCred(conn)` 经 `syscall.GetsockoptUcred(SOL_SOCKET, SO_PEERCRED)` 取凭据；本实现
//!   经 `tokio::net::UnixListener` 的 `peer_cred()`（标准库 `UCred` 的一等原生 API，无 unsafe）。
//! - Go 源 `isAuthorized(uid)` 逐行解析 authfile；本实现读文件后按行 split + parse，行为等价。
//! - 安全模型：root(0) 恒授权；authfile 缺失时非 root 一律失败安全（返回 false）。
//!
//! 所有系统操作经 [`PeerCredProvider`] trait 抽象，测试用 [`StaticPeerCred`] 桩注入伪造 uid。

use std::path::Path;

/// 对端进程凭据（uid/gid），移植自 Go `syscall.Ucred`（内核在 connect 时锁定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    /// 对端进程 uid（鉴权与 setuid 的唯一依据）。
    pub uid: u32,
    /// 对端进程 gid（setuid 拉核时 setgid 目标）。
    pub gid: u32,
}

/// 取对端凭据的抽象（trait 便于测试 mock；生产用 [`TokioPeerCred`]）。
///
/// 等价 Go `peerCred(conn)`：经 SO_PEERCRED 取不可伪造的 uid/gid。
pub trait PeerCredProvider {
    /// 返回对端 uid/gid；失败（非 unix conn / Getsockopt 失败）返回 None → 上层报 `ERR peercred`。
    fn peer_cred(&self) -> Option<PeerCred>;
}

/// 鉴权错误码（对应 wire 协议 `ERR peercred` / `ERR unauthorized`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// 取不到对端凭据（非 unix conn / Getsockopt 失败）—— `helper-linux/helper.go:339`。
    #[error("peercred")]
    Peercred,
    /// uid 不在授权列表 —— `helper-linux/helper.go:355`。
    #[error("unauthorized")]
    Unauthorized,
}

impl AuthError {
    /// 对应的 wire 错误码 token（逐字对照 Go 源 `ERR peercred` / `ERR unauthorized`）。
    #[must_use]
    pub const fn wire_token(self) -> &'static str {
        match self {
            Self::Peercred => "peercred",
            Self::Unauthorized => "unauthorized",
        }
    }
}

/// 静态凭据桩（测试用：注入伪造 uid/gid 验证授权逻辑，不碰真实 socket）。
#[derive(Debug, Clone, Copy)]
pub struct StaticPeerCred {
    cred: PeerCred,
}

impl StaticPeerCred {
    #[must_use]
    pub const fn new(uid: u32, gid: u32) -> Self {
        Self {
            cred: PeerCred { uid, gid },
        }
    }
}

impl PeerCredProvider for StaticPeerCred {
    fn peer_cred(&self) -> Option<PeerCred> {
        Some(self.cred)
    }
}

/// 取不到凭据的桩（模拟 SO_PEERCRED 失败分支，验证 `ERR peercred` 路径）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPeerCred;

impl PeerCredProvider for NoPeerCred {
    fn peer_cred(&self) -> Option<PeerCred> {
        None
    }
}

/// tokio::net::UnixStream 的 SO_PEERCRED 实现（生产用）。
#[derive(Debug, Clone)]
pub struct TokioPeerCred<'a> {
    stream: &'a tokio::net::UnixStream,
}

impl<'a> TokioPeerCred<'a> {
    #[must_use]
    pub const fn new(stream: &'a tokio::net::UnixStream) -> Self {
        Self { stream }
    }
}

impl PeerCredProvider for TokioPeerCred<'_> {
    fn peer_cred(&self) -> Option<PeerCred> {
        // tokio::net::UnixStream::peer_cred() 内部走 SO_PEERCRED（Linux）/ getpeereid（macOS）。
        // 返回 tokio::net::unix::UCred —— 内核背书，不可伪造。
        self.stream.peer_cred().ok().map(|c| PeerCred {
            uid: c.uid(),
            gid: c.gid(),
        })
    }
}

/// 已捕获的对端凭据（生产 accept 循环用）。
///
/// 生产连接处理器在 accept 时先从 tokio `UnixStream` 取 SO_PEERCRED（[`TokioPeerCred`]），再把 async 流
/// 转 std 阻塞流交给同步 [`handle`](crate::platform::linux::handle)。转换后原 stream 已被消费，无法再取凭据，
/// 故把凭据**捕获**进本类型随 `handle` 下发。`None` = 取凭据失败（非 unix conn / getsockopt 失败）→
/// `handle` 走 `ERR peercred` 分支（与 Go `peerCred(conn)` 失败一致）。
#[derive(Debug, Clone, Copy)]
pub struct CapturedPeerCred(pub Option<PeerCred>);

impl PeerCredProvider for CapturedPeerCred {
    fn peer_cred(&self) -> Option<PeerCred> {
        self.0
    }
}

// ===== 授权 uid 列表（移植自 Go isAuthorized）=====

/// 判定 uid 是否在授权列表。root(0) 恒授权（`helper-linux/helper.go:97-99`）。
///
/// authfile 每行一个十进制 uid；缺失/读取失败时非 root 一律未授权（失败安全，:101）。
/// 空行与非法行静默跳过（:104-113）。
#[must_use]
pub fn is_authorized(uid: u32, auth_file: &Path) -> bool {
    // root 恒授权（Go: if uid == 0 { return true }）。
    if uid == 0 {
        return true;
    }
    let Ok(data) = std::fs::read_to_string(auth_file) else {
        // 缺文件 → 非 root 失败安全（Go: err != nil → return false）。
        return false;
    };
    data.lines().any(|line| parse_uid_line(line) == Some(uid))
}

/// 解析单行为 uid；空行/非法行返回 None（对照 Go TrimSpace + Atoi + >=0 校验）。
fn parse_uid_line(line: &str) -> Option<u32> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    // 仅接受纯非负十进制（Go: strconv.Atoi + n >= 0；负号已被 parse 拒绝）。
    let n: u32 = t.parse().ok()?;
    Some(n)
}

/// 校验路径属主 == uid（移植自 Go `ownedBy`，:117-133）。
///
/// 用 `open` + `fstat`（而非 `stat(path)`）防 TOCTOU：`stat(path)` 校验通过后、拉核 execve 读 config 前，
/// 攻击者把 path 换成别人的文件（symlink swap / rename）→ helper 会以对端 uid 拉核读到本不属于它的配置。
/// `File::open` 拿到 fd 后 `fstat` 该 **fd**（非 path），校验的属主与后续被读的是同一 inode，杜绝换靶。
/// `File::open` 默认跟随 symlink 到目标（与 Go `os.Open` 一致）。
///
/// 返回 `Ok(true)` = 属主匹配；`Ok(false)` = 属主不匹配；`Err` = open/fstat 失败（路径不存在等）。
pub fn owned_by(path: &Path, uid: u32) -> Result<bool, std::io::Error> {
    use std::os::unix::fs::MetadataExt;
    // Go: f, err := os.Open(path); ...; fi, err := f.Stat()（对 *os.File 的 Stat = fstat(fd)）。
    let f = std::fs::File::open(path)?;
    // File::metadata() 走 fstat(fd)（非 stat(path)）—— TOCTOU 关键：校验对象 == 后续被读的 inode。
    let meta = f.metadata()?;
    Ok(meta.uid() == uid)
}

/// 登录用户 `uid` 的补充组 gid 列表（移植自 Go `supplementaryGroups`，:135-155）。
///
/// setuid 拉核时随 [`SpawnCoreRequest`](crate::platform::linux::state::SpawnCoreRequest) 下发给 `setgroups`：
/// 否则降权后默认 `setgroups(0)` 清空补充组，核读不到 group-only 资源（ssl-cert 组证书 / 组共享规则文件等），
/// 而 app 直起路径（保留补充组）能读，造成 mode-specific 破坏。
///
/// 经 `nix::unistd::User::from_uid`（getpwuid_r）取登录名 + 主组，再 `getgrouplist` 取全部所属组
/// （Go `user.LookupId(uid).GroupIds()` 的 `user::*` 等价）。查不到 → 空 Vec（Go 返回 nil：退化为
/// `setgroups(&[])` 清空，不比修前差）。**在 fork 前于父进程解析**（结果进 request）—— 拉核子进程的
/// `pre_exec` 只做 syscall、不碰 NSS/分配，降低 fork-child async-signal-safety 风险。
///
/// **合理差异（Go oracle 对照）**：Go 注释注明 CGO 关时 `GroupIds()` 纯解析 `/etc/group`（不含 NSS/SSSD）；
/// 本实现 `getgrouplist` 走 libc NSS。是**跨编译约束**（Go 的 CGO-off）非安全要求，NSS 解析对 LDAP/SSSD
/// 部署更正确（严于原版，非缺陷）；语义（返回用户所属全部组）一致。
#[must_use]
pub fn supplementary_groups(uid: u32) -> Vec<u32> {
    // Go: u, err := user.LookupId(uid); if err != nil { return nil }
    let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) else {
        return Vec::new();
    };
    // getgrouplist 需 &CStr 登录名；含 NUL（真实用户名不可能）→ 退化空。
    let Ok(name) = std::ffi::CString::new(user.name) else {
        return Vec::new();
    };
    // Go: gidStrs, err := u.GroupIds(); if err != nil { return nil }
    match nix::unistd::getgrouplist(&name, user.gid) {
        Ok(gids) => gids.into_iter().map(nix::unistd::Gid::as_raw).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use std::fs::write;
    use tempfile::tempdir;

    // ===== is_authorized（逐字对照 Go TestIsAuthorized）=====

    #[test]
    fn root_always_authorized_even_without_authfile() {
        // Go: if uid == 0 { return true } —— root 不依赖 authfile 存在性。
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent");
        assert!(is_authorized(0, &missing), "root 应恒授权");
    }

    #[test]
    fn listed_uids_authorized() {
        // Go TestIsAuthorized: authfile = "1000\n1001\n\n"
        let dir = tempdir().unwrap();
        let f = dir.path().join("auth");
        write(&f, "1000\n1001\n\n").unwrap();
        assert!(is_authorized(1000, &f));
        assert!(is_authorized(1001, &f));
    }

    #[test]
    fn unlisted_uid_not_authorized() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("auth");
        write(&f, "1000\n1001\n").unwrap();
        assert!(!is_authorized(1002, &f), "uid 1002 不在列表");
    }

    #[test]
    fn missing_authfile_fails_closed_for_non_root() {
        // 失败安全：authfile 缺失时非 root 一律未授权（Go: err != nil → return false）。
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent");
        assert!(!is_authorized(1000, &missing));
        assert!(is_authorized(0, &missing), "root 仍授权");
    }

    #[test]
    fn blank_and_garbage_lines_skipped() {
        // Go: 空行 / 非数字行 continue（静默跳过，不报错）。
        let dir = tempdir().unwrap();
        let f = dir.path().join("auth");
        write(&f, "\n1000\n\nnot-a-number\n  1001  \n").unwrap();
        assert!(is_authorized(1000, &f));
        assert!(is_authorized(1001, &f), "带空白的行应 TrimSpace 后通过");
        assert!(!is_authorized(1002, &f));
    }

    #[test]
    fn negative_uid_string_rejected() {
        // Go strconv.Atoi("-1") = -1，但 n >= 0 校验通过后 uint32(-1) != 任何 uid。
        // 本实现 u32::parse 直接拒绝负号 → None（更严格，语义等价：不授权）。
        let dir = tempdir().unwrap();
        let f = dir.path().join("auth");
        write(&f, "-1\n").unwrap();
        assert!(!is_authorized(u32::MAX, &f), "负数 uid 串不应匹配任何 uid");
        assert!(is_authorized(0, &f), "root 仍授权");
    }

    // ===== owned_by（逐字对照 Go TestOwnedBy）=====

    #[test]
    fn owned_by_self_for_self_created_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("x");
        write(&f, "y").unwrap();
        let self_uid = current_uid();
        assert!(owned_by(&f, self_uid).unwrap(), "本进程 uid 应拥有自建文件");
    }

    #[test]
    fn owned_by_wrong_uid_returns_false() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("x");
        write(&f, "y").unwrap();
        // 用一个极不可能匹配的 uid。
        assert!(
            !owned_by(&f, current_uid().wrapping_add(9999)).unwrap(),
            "错误 uid 不应通过属主校验"
        );
    }

    #[test]
    fn owned_by_missing_path_returns_err() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("none");
        assert!(
            owned_by(&missing, current_uid()).is_err(),
            "不存在的路径应返回 err"
        );
    }

    /// 取当前进程 uid（测试 helper，对齐 Go os.Getuid）。
    fn current_uid() -> u32 {
        // nix::unistd::getuid 是 getuid(2) 的 safe wrapper（forbid(unsafe_code) 下替代 libc::getuid 的 unsafe FFI）。
        nix::unistd::getuid().as_raw()
    }

    // ===== PeerCredProvider 桩 =====

    #[test]
    fn static_peer_cred_returns_injected() {
        let p = StaticPeerCred::new(1000, 2000);
        let c = p.peer_cred().unwrap();
        assert_eq!(
            c,
            PeerCred {
                uid: 1000,
                gid: 2000
            }
        );
    }

    #[test]
    fn no_peer_cred_returns_none() {
        let p = NoPeerCred;
        assert!(p.peer_cred().is_none(), "NoPeerCred 模拟 SO_PEERCRED 失败");
    }

    #[test]
    fn captured_peer_cred_carries_or_reports_failure() {
        // Some(cred) → 原样透传（accept 时捕获的 SO_PEERCRED）。
        let ok = CapturedPeerCred(Some(PeerCred {
            uid: 1000,
            gid: 1000,
        }));
        assert_eq!(
            ok.peer_cred(),
            Some(PeerCred {
                uid: 1000,
                gid: 1000
            })
        );
        // None → handle 走 ERR peercred（凭据捕获失败）。
        let bad = CapturedPeerCred(None);
        assert!(bad.peer_cred().is_none());
    }

    // ===== owned_by 的 open+fstat 语义（TOCTOU 修复）=====

    #[test]
    fn owned_by_follows_symlink_to_target_owner() {
        // File::open 跟随 symlink 到目标（Go os.Open 语义）；owned_by 校验的是**目标 inode** 属主。
        let dir = tempdir().unwrap();
        let target = dir.path().join("real_cfg.json");
        write(&target, b"{}").unwrap();
        let link = dir.path().join("link_cfg.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // 经 symlink 校验 → 跟随到 target（本进程 uid 拥有）。
        assert!(
            owned_by(&link, current_uid()).unwrap(),
            "symlink 应跟随到目标 inode 校验属主"
        );
    }

    // ===== supplementary_groups（getgrouplist 解析，对照 Go supplementaryGroups）=====

    #[test]
    fn supplementary_groups_current_uid_contains_primary() {
        // getgrouplist 对当前（非特权）uid 返回其所属全部组，必含主组（Go GroupIds 语义）。
        let uid = current_uid();
        let gids = supplementary_groups(uid);
        let primary = nix::unistd::getgid().as_raw();
        assert!(
            gids.contains(&primary),
            "补充组列表应含主组 gid={primary}，got {gids:?}"
        );
    }

    #[test]
    fn supplementary_groups_unknown_uid_returns_empty() {
        // 极不可能存在的 uid → LookupId 失败 → 空 Vec（Go: err != nil → nil）。
        let gids = supplementary_groups(4_000_000_000);
        assert!(gids.is_empty(), "未知 uid 应返回空组列表，got {gids:?}");
    }

    #[test]
    fn auth_error_wire_tokens_match_go_source() {
        // 逐字对照 Go 源 fmt.Fprintln(conn, "ERR peercred" / "ERR unauthorized")。
        assert_eq!(AuthError::Peercred.wire_token(), "peercred");
        assert_eq!(AuthError::Unauthorized.wire_token(), "unauthorized");
        // wire_token 与 polaris-helper-proto 的 ErrorCode 双向一致。
        use polaris_helper_proto::ErrorCode;
        assert_eq!(
            ErrorCode::from_wire_token(AuthError::Peercred.wire_token()),
            ErrorCode::Peercred
        );
        assert_eq!(
            ErrorCode::from_wire_token(AuthError::Unauthorized.wire_token()),
            ErrorCode::Unauthorized
        );
    }
}
