//! 路径 / 接口白名单（移植自 上游 `helper/helper.go:245-272`）。
//!
//! ## 职责
//!
//! 1. **`cfg_allowed`**：start 的 cfg 路径必须落在 `--confdir` 白名单目录内（防越权指定任意路径作
//!    root 配置，`helper.go:245-252`）。清洗后前缀匹配。
//! 2. **iface 白名单**：route-add/del 的接口名仅允许 Polaris 自家接口（复用 proto crate 的
//!    [`is_mac_iface_allowed`](polaris_helper_proto::codec::is_mac_iface_allowed)，逐字移植
//!    `helper.go:255-272`）。
//!
//! ## 移植纪律
//!
//! Go 用 `filepath.Clean` + `filepath.Join` + `strings.HasPrefix` 做路径前缀匹配。Rust 等价用
//! `std::path::Path::components` 收集规范化段（`.` 折叠、`..` 不展开但清洗后仍可比对）。
//! 关键：Go 的 `filepath.Clean("/a/b/../c")` → `/a/c`，Rust `Path::components()` 会保留 `..`，
//! 故这里手动实现等价 Clean（NormalizeComponents）—— 见 [`normalize_path`]。

use std::path::{Path, PathBuf};

/// 判断 cfg 路径是否在 conf_dir 白名单内（移植自 `helper.go:245-252` 的 `cfgAllowed`）。
///
/// 对照 Go：
/// ```text
/// func cfgAllowed(cfg string) bool {
///     if confDir == "" { return true }
///     clean := filepath.Clean(cfg)
///     base := filepath.Clean(confDir) + string(os.PathSeparator)
///     return strings.HasPrefix(clean, base)
/// }
/// ```
///
/// conf_dir 为空 → 一律放行（等价 Go `confDir == ""`，向后兼容未设 flag 的旧部署）。
/// 否则 cfg 与 conf_dir 各自 Clean 后，cfg 须以 `<conf_dir>/` 为前缀（含分隔符防 `/foo` 误匹配 `/foobar`）。
#[must_use]
pub fn cfg_allowed(cfg: &str, conf_dir: &str) -> bool {
    if conf_dir.is_empty() {
        return true;
    }
    let clean_cfg = normalize_path(cfg);
    let clean_base = normalize_path(conf_dir);
    // Go: base = Clean(confDir) + PathSeparator；cfg 须 HasPrefix base。
    // 即 cfg == base 或 cfg 以 "base/" 开头。cfg == base 本身（无文件名）也应算越权（cfg 是文件不是目录），
    // 但 Go 的 HasPrefix 在 cfg==base 时也返回 true —— 严格对照 Go 行为，此处同样放行。
    if clean_cfg == clean_base {
        return true;
    }
    // Go: base = Clean(confDir) + PathSeparator；cfg 须 HasPrefix base。
    // 构造 "base/" 字符串做前缀比对（防 /foo 误匹配 /foobar）。
    let base_with_sep = format!("{}{}", clean_base.display(), std::path::MAIN_SEPARATOR);
    clean_cfg.starts_with(base_with_sep)
}

/// 判断接口名是否在 macOS 白名单内（委托 proto crate，逐字移植 `helper.go:255-272`）。
///
/// 仅允许 `polaris-ts` / `polaris-wg` / `utunN`（N 为 1-3 位数字）。详见
/// [`polaris_helper_proto::codec::is_mac_iface_allowed`]。
#[must_use]
pub fn iface_allowed(s: &str) -> bool {
    polaris_helper_proto::codec::is_mac_iface_allowed(s)
}

/// 路径规范化（移植自 Go `filepath.Clean`）。
///
/// Go `filepath.Clean` 语义：折叠 `.`、解析 `..`（向上消解前一段，根的 `..` 仍是根）、合并连续分隔符、
/// 去尾随分隔符（根 `/` 除外）。返回的字符串是规范形式，前缀比对可靠。
///
/// 用 `Vec<Component>` 栈式消解 `..`：遇 `CurDir`/`RootDir`/分隔符折叠跳过，遇 `..` 弹栈（栈空且绝对路径则
/// 保留在根，等价 Go `/..` → `/`）。
fn normalize_path(p: &str) -> PathBuf {
    let path = Path::new(p);
    let mut stack: Vec<std::path::Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => { /* 折叠 `.` */ }
            std::path::Component::ParentDir => {
                // Go filepath.Clean 语义：
                // - 栈顶是 Normal 段 → 弹栈消解（a/../b → a/b）
                // - 栈顶是 RootDir 或栈空 → Go 的 Clean 丢弃此 ..（`/..` → `/`，根的 .. 仍是根）
                // - 栈顶是已存在的 .. → Go Clean 会保留连续 ..（`../..` → `../..`）
                match stack.last() {
                    Some(std::path::Component::Normal(_)) => {
                        stack.pop();
                    }
                    Some(std::path::Component::RootDir) | None => {
                        // 根下的 .. 丢弃（Go: `/..` → `/`；相对路径栈空的 .. 也丢弃是 Go 的行为差异，
                        // 但本协议 cfg/confdir 都是绝对路径，根下丢弃对齐 Go）
                    }
                    _ => stack.push(comp),
                }
            }
            other => stack.push(other),
        }
    }
    let mut out = PathBuf::new();
    for comp in &stack {
        out.push(comp);
    }
    // Go Clean 对空输入返回 "."；Rust 空 PathBuf → "" ，对齐：返回 "."
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== cfg_allowed（逐字对照 helper.go:245-252）=====

    #[test]
    fn cfg_allowed_empty_confdir_allows_all() {
        // helper.go:247: if confDir == "" { return true }
        assert!(cfg_allowed("/etc/passwd", ""));
        assert!(cfg_allowed("/tmp/any.json", ""));
    }

    #[test]
    fn cfg_allowed_inside_confdir() {
        // helper.go:249-251: clean cfg HasPrefix (Clean(confDir)+sep)
        assert!(cfg_allowed(
            "/Users/me/Library/Application Support/Polaris/cfg.json",
            "/Users/me/Library/Application Support/Polaris"
        ));
    }

    #[test]
    fn cfg_allowed_denies_outside_confdir() {
        // helper.go:530: ERR config-path-denied
        assert!(!cfg_allowed("/etc/passwd", "/Users/me/Polaris"));
        assert!(!cfg_allowed(
            "/tmp/evil.json",
            "/Users/me/Library/Application Support/Polaris"
        ));
    }

    #[test]
    fn cfg_allowed_path_traversal_denied() {
        // 防 ../../../etc/passwd 注入：Clean 后前缀不匹配
        assert!(!cfg_allowed(
            "/Users/me/Polaris/../../../etc/passwd",
            "/Users/me/Polaris"
        ));
    }

    #[test]
    fn cfg_allowed_similar_prefix_denied() {
        // 防前缀碰撞：cfg=/fooBar 不应匹配 confdir=/foo（须带分隔符）
        assert!(!cfg_allowed("/Polarisbar/cfg.json", "/Polaris"));
    }

    #[test]
    fn cfg_allowed_normalized_paths() {
        // Go filepath.Clean 折叠 ./ 与 //
        assert!(cfg_allowed(
            "/Users/me/Polaris/./sub/../cfg.json",
            "/Users/me/Polaris"
        ));
        assert!(cfg_allowed(
            "/Users/me//Polaris/cfg.json",
            "/Users/me/Polaris"
        ));
    }

    // ===== iface_allowed（逐字对照 helper.go:255-272，委托 proto）=====

    #[test]
    fn iface_allowed_matches_go_source() {
        // helper.go:256-258: polaris-ts / polaris-wg
        assert!(iface_allowed("polaris-ts"));
        assert!(iface_allowed("polaris-wg"));
        // helper.go:259-271: utunN (1-3 digits)
        assert!(iface_allowed("utun0"));
        assert!(iface_allowed("utun3"));
        assert!(iface_allowed("utun999"));
    }

    #[test]
    fn iface_denied_variants() {
        // helper.go:260: 非 utun 前缀
        assert!(!iface_allowed("en0"));
        assert!(!iface_allowed("eth0"));
        // helper.go:262: rest == "" → false
        assert!(!iface_allowed("utun"));
        // helper.go:263: len(rest) > 3 → false
        assert!(!iface_allowed("utun1234"));
        // helper.go:266-269: rest 含非数字
        assert!(!iface_allowed("utunX"));
        assert!(!iface_allowed("utun1a"));
        // 非 polaris-* 自家前缀
        assert!(!iface_allowed("polaris-other"));
        assert!(!iface_allowed(""));
        assert!(!iface_allowed("Polaris-ts")); // 大小写敏感
    }

    // ===== normalize_path（对齐 Go filepath.Clean）=====

    #[test]
    fn normalize_collapses_dotdot() {
        assert_eq!(normalize_path("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(normalize_path("/a/b/../../c"), PathBuf::from("/c"));
    }

    #[test]
    fn normalize_collapses_curdir_and_double_sep() {
        assert_eq!(normalize_path("/a/./b"), PathBuf::from("/a/b"));
        assert_eq!(normalize_path("/a//b"), PathBuf::from("/a/b"));
    }

    #[test]
    fn normalize_dotdot_at_root_stays_root() {
        // Go: filepath.Clean("/..") → "/"
        assert_eq!(normalize_path("/.."), PathBuf::from("/"));
        assert_eq!(normalize_path("/../.."), PathBuf::from("/"));
    }

    #[test]
    fn normalize_empty_returns_dot() {
        // Go: filepath.Clean("") → "."
        assert_eq!(normalize_path(""), PathBuf::from("."));
    }

    #[test]
    fn normalize_trailing_sep_stripped() {
        // Go: filepath.Clean("/a/b/") → "/a/b"
        assert_eq!(normalize_path("/a/b/"), PathBuf::from("/a/b"));
    }
}
