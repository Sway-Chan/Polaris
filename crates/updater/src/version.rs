//! sing-box / app 版本解析的单一权威（移植自 上游 `shared/version.ts`）。
//!
//! 上游 `version.ts` 注释记录的历史问题：配置生成层曾用 `parseFloat("1." + minor)` 判断版本带，
//! 把 minor 当小数 —— `"1.9"` → 1.9 被误判为 > 1.13，`"1.20"` → 1.2 被误判为 < 1.13。
//! 核心更新层早已用整数编码避开该坑。本模块把 Polaris 两处散落的版本逻辑（CoreUpdateService.compareVersions
//! + UpdateService.isNewerVersion）统一到这里的纯函数，与 上游 `compareSemver` 行为逐字一致。
//!
//! 锚点：`src/shared/version.ts` 全文。

use thiserror::Error;

/// 版本解析/比较失败（目前仅「空版本」属于硬错误；不可解析的段在 [`compare_semver`] 中按 0 计而非报错，
/// 与 上游 `parseInt(p, 10) || 0` 同口径）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseVersionError {
    /// 版本字符串为空（上游 `encodeMajorMinor` 对 `""` 返回 NaN；此处显式报错以便调用方区分）。
    #[error("empty version string")]
    Empty,
}

/// 把版本字符串编码为可比较整数 `major * 1000 + minor`。
///
/// 移植自 `version.ts:encodeMajorMinor`：
///   - `"1.20.3"` → 1020
///   - `"v1.13.13"` → 1013
///   - `"1.9.0"` → 1009
///
/// 容忍前导 `v` 与任意后缀（`-beta` / `+naive` 等）。无法解析返回 [`None`]（对齐 Polaris 的 NaN 语义，
/// 失败安全：调用方据此跳过比较，而非把 NaN 当成可比较值）。
#[must_use]
pub fn encode_major_minor(version: &str) -> Option<u32> {
    // 对齐 上游 `/^v?(\d+)\.(\d+)/`：前导可选 v，后跟「数字.数字」，忽略之后一切。
    let s = version.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    major.checked_mul(1000)?.checked_add(minor)
}

/// 两版本是否同 `major.minor`（兼容版本带内）。
///
/// 移植自 `version.ts:sameMajorMinor`：
///   - `"1.13.13"` vs `"1.13.14"` → true
///   - `"1.13.x"` vs `"1.14.x"` → false
///
/// 任一无法解析（含 `"未知"`）→ [`None`] → 调用方按失败安全处理（Polaris 原义：内核自动更新跨带硬闸
/// 宁可不更，绝不误判同带跨 minor 落位）。返回 [`Option`] 而非 Polaris 的 `false`：让调用方显式区分
/// 「确认不同带」与「无法判定」。
#[must_use]
pub fn same_major_minor(a: &str, b: &str) -> Option<bool> {
    let ea = encode_major_minor(a)?;
    let eb = encode_major_minor(b)?;
    Some(ea == eb)
}

/// 解析出主版本段数组与 prerelease 段数组（build `+...` 直接丢弃，不参与比较）。
///
/// 移植自 `version.ts:compareSemver` 内部 `parse`。缺失或非数字主版本段按 0 计
/// （上游 `parseInt(p, 10) || 0`）。
fn parse_semver(v: &str) -> (Vec<u64>, Vec<String>) {
    let stripped = v.trim().trim_start_matches(['v', 'V']);
    // build `+...` 丢弃（semver 规范：build metadata 不参与优先级）。
    let no_build = stripped.split('+').next().unwrap_or_default();
    let (main_str, pre_str) = no_build
        .split_once('-')
        .map_or((no_build, ""), |(m, p)| (m, p));

    let main: Vec<u64> = main_str
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect();
    // 空 pre_str → 空 Vec（=「无 prerelease」）；非空 → 按 '.' 切分。
    let pre: Vec<String> = if pre_str.is_empty() {
        Vec::new()
    } else {
        pre_str.split('.').map(ToString::to_string).collect()
    };
    (main, pre)
}

/// 比较两 prerelease 段标识符（semver 规范）。
///
/// 移植自 `version.ts` prerelease 段比较规则：
///   - 纯数字段按数值比（`1` < `2`）
///   - 含字母段按 ASCII 字典序（`alpha` < `beta`）
///   - 数字段 < 字母段（`1` < `alpha`）
///
/// 返回 [`core::cmp::Ordering`]：[`Equal`](core::cmp::Ordering::Equal) 仅当两标识符同型且同值。
fn cmp_pre_ident(a: &str, b: &str) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a.parse::<u64>(), b.parse::<u64>()) {
        // 两段都是纯数字 → 数值比较。
        (Ok(na), Ok(nb)) => na.cmp(&nb),
        // a 数字、b 字母 → a < b。
        (Ok(_), Err(_)) => Ordering::Less,
        // a 字母、b 数字 → a > b。
        (Err(_), Ok(_)) => Ordering::Greater,
        // 两段都是字母 → ASCII 字典序。
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// 比较两 prerelease 段数组（semver 规范）。
///
/// 移植自 `version.ts` prerelease 比较：逐段比，首个差异决定优先级；段数不同时**较长者更大**
/// （`alpha` < `alpha.1`）。两数组都为空（即都无 prerelease）→ [`Equal`](core::cmp::Ordering::Equal)。
fn cmp_pre(a: &[String], b: &[String]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (pa, pb) in a.iter().zip(b.iter()) {
        let o = cmp_pre_ident(pa, pb);
        if o != Ordering::Equal {
            return o;
        }
    }
    // 公共前缀全等 → 段数多者更大（`alpha` < `alpha.1`）。
    a.len().cmp(&b.len())
}

/// 健壮三段（及以上）语义版本比较的单一权威（移植自 `version.ts:compareSemver`）。
///
/// 返回值：`>0` → `a` 更新（= Polaris 返回 `1`）；`<0` → `b` 更新（= Polaris 返回 `-1`）；`0` → 相等。
///
/// 行为（逐字对齐 上游 `compareSemver` 注释）：
///   - 容忍前导 `v`（`"v1.13.13"`）与 build 后缀（`+naive`，比较时忽略）
///   - 支持 semver prerelease 优先级：
///     - 主版本号（major.minor.patch…）数字段优先逐段比较
///     - 主版本相同时「有 prerelease」< 「无 prerelease」（`1.14.0-alpha.32 < 1.14.0`）
///     - 两者都有 prerelease 时逐段比 prerelease 标识符（数字段按数字、字母段按 ASCII、数字段 < 字母段）
///     - 段数不同时较长者更大（`alpha < alpha.1`）
///   - 由此 `alpha.32 < alpha.33 < beta.1 < rc.1 < 1.14.0 < 1.14.1` 全链成立
///   - 每段缺失或非数字主版本段按 0 计；build（`+`）后缀不参与比较（semver 规范）
///
/// # Errors
///
/// 仅当 `a` 或 `b` 为空串时返回 [`ParseVersionError::Empty`]（Polaris 原实现容忍空串但产出无意义比较结果；
/// 显式报错让调用方不至于在「空 vs 空」上误判「相等」进而漏更新）。
pub fn compare_semver(a: &str, b: &str) -> Result<i32, ParseVersionError> {
    if a.trim().is_empty() {
        return Err(ParseVersionError::Empty);
    }
    if b.trim().is_empty() {
        return Err(ParseVersionError::Empty);
    }

    let (main_a, pre_a) = parse_semver(a);
    let (main_b, pre_b) = parse_semver(b);

    // 1) 主版本段逐段比较（缺失段按 0）。
    let max_len = main_a.len().max(main_b.len());
    for i in 0..max_len {
        let na = main_a.get(i).copied().unwrap_or(0);
        let nb = main_b.get(i).copied().unwrap_or(0);
        match na.cmp(&nb) {
            core::cmp::Ordering::Equal => continue,
            core::cmp::Ordering::Greater => return Ok(1),
            core::cmp::Ordering::Less => return Ok(-1),
        }
    }

    // 2) 主版本全等 → 比较 prerelease：
    //    - 都无 pre（空）→ Equal（0）
    //    - a 有 pre、b 无 pre → a < b（ prerelease 优先级低于正式版）
    //    - a 无 pre、b 有 pre → a > b
    //    - 都有 pre → 逐段比 prerelease
    use core::cmp::Ordering;
    let ord = match (pre_a.is_empty(), pre_b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater, // a 无 pre > b 有 pre
        (false, true) => Ordering::Less,    // a 有 pre < b 无 pre
        (false, false) => cmp_pre(&pre_a, &pre_b),
    };
    Ok(match ord {
        Ordering::Equal => 0,
        Ordering::Greater => 1,
        Ordering::Less => -1,
    })
}

/// 「a 是否比 b 新」（= 上游 `UpdateService.isNewerVersion` / `CoreUpdateService.compareVersions` > 0 的薄包装）。
///
/// 不可解析（空串等）按 [`ParseVersionError`] 透传，调用方可据此按失败安全处理（Polaris 原实现 `compareSemver`
/// 对非空串恒返回数值；此处仅对空串报错，其余与 Polaris 完全一致）。
///
/// # Errors
///
/// 透传 [`compare_semver`] 的错误。
pub fn is_newer(a: &str, b: &str) -> Result<bool, ParseVersionError> {
    Ok(compare_semver(a, b)? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_major_minor_basic() {
        // 移植自 version.ts:encodeMajorMinor 注释用例。
        assert_eq!(encode_major_minor("1.20.3"), Some(1020));
        assert_eq!(encode_major_minor("v1.13.13"), Some(1013));
        assert_eq!(encode_major_minor("1.9.0"), Some(1009));
        // 容忍前缀 v（大小写）+ 任意后缀。
        assert_eq!(encode_major_minor("V1.14.0-alpha.32+naive"), Some(1014));
    }

    #[test]
    fn encode_major_minor_unparseable() {
        // Polaris 对这些返回 NaN；此处返回 None（失败安全）。
        assert_eq!(encode_major_minor("未知"), None);
        assert_eq!(encode_major_minor(""), None);
        assert_eq!(encode_major_minor("abc"), None);
    }

    #[test]
    fn same_major_minor_band() {
        // 移植自 version.ts:sameMajorMinor 注释用例。
        assert_eq!(same_major_minor("1.13.13", "1.13.14"), Some(true));
        assert_eq!(same_major_minor("1.13.x", "1.14.x"), Some(false));
        // 「1.x」「2.x」minor 段非数字 → 不可解析 → None（Polaris 正则 /^v?(\d+)\.(\d+)/ 同样得 NaN）。
        assert_eq!(same_major_minor("1.x", "2.x"), None);
        // 任一不可解析 → None（Polaris 原义 false，但调用方需区分「确认不同带」与「无法判定」）。
        assert_eq!(same_major_minor("未知", "1.13.13"), None);
    }

    #[test]
    fn compare_semver_basic() {
        // 移植自 version.ts:compareSemver 注释链：alpha.32 < alpha.33 < beta.1 < rc.1 < 1.14.0 < 1.14.1
        assert_eq!(compare_semver("1.14.0-alpha.32", "1.14.0-alpha.33"), Ok(-1));
        assert_eq!(compare_semver("1.14.0-alpha.33", "1.14.0-beta.1"), Ok(-1));
        assert_eq!(compare_semver("1.14.0-beta.1", "1.14.0-rc.1"), Ok(-1));
        assert_eq!(compare_semver("1.14.0-rc.1", "1.14.0"), Ok(-1));
        assert_eq!(compare_semver("1.14.0", "1.14.1"), Ok(-1));
        // 反向 → 1。
        assert_eq!(compare_semver("1.14.1", "1.14.0"), Ok(1));
        // 相等 → 0。
        assert_eq!(compare_semver("1.14.0", "1.14.0"), Ok(0));
    }

    /// **随包基线升级用例（跨 alpha→beta 标识符）**：`alpha.45` 必须 < `beta.3`。
    ///
    /// 为什么单列一条而不靠上面的 `alpha.33 < beta.1`：那一对的数字段恰好也是升序（33→1 除外，
    /// 但首段 `alpha`/`beta` 就已定序），**掩盖不了**「只比数字段」这类退化——本对的数字段是
    /// **降序**（45 → 3），只有真按 semver「先比首个标识符（字母段 ASCII）」才能得 `-1`。
    ///
    /// 变异锁：把 [`cmp_pre`] 改成先比末段数字（或整体丢掉 prerelease 比较只比主版本段）→
    /// 本用例得 `1`（或 `0`）→ 转红。它也是 `core_paths::decide_reseed` 判「随包核更新 ⇒ 重播种」
    /// 的算术前提：判反了就是「装了新包仍跑旧核」。
    #[test]
    fn compare_semver_alpha_to_beta_ignores_numeric_suffix_order() {
        assert_eq!(compare_semver("1.14.0-alpha.45", "1.14.0-beta.3"), Ok(-1));
        assert_eq!(compare_semver("1.14.0-beta.3", "1.14.0-alpha.45"), Ok(1));
        // 同一标识符内仍按数值（不是字典序：`beta.3` < `beta.10`，字典序会判反）。
        assert_eq!(compare_semver("1.14.0-beta.3", "1.14.0-beta.10"), Ok(-1));
    }

    #[test]
    fn compare_semver_prefix_and_build() {
        // 容忍前导 v。
        assert_eq!(compare_semver("v1.13.13", "1.13.13"), Ok(0));
        // build (+...) 后缀不参与比较（semver 规范）。
        assert_eq!(compare_semver("1.14.0+naive", "1.14.0+other"), Ok(0));
        // 不同主版本。
        assert_eq!(compare_semver("2.0.0", "1.99.99"), Ok(1));
    }

    #[test]
    fn compare_semver_missing_segments() {
        // 缺失段按 0 计（上游 `parseInt(p, 10) || 0`）。
        assert_eq!(compare_semver("1.14", "1.14.0"), Ok(0));
        assert_eq!(compare_semver("1.14.0.1", "1.14.0"), Ok(1));
    }

    #[test]
    fn compare_semver_prerelease_segment_count() {
        // 段数多者更大（alpha < alpha.1）。
        assert_eq!(compare_semver("1.0.0-alpha", "1.0.0-alpha.1"), Ok(-1));
        // 数字段 < 字母段。
        assert_eq!(compare_semver("1.0.0-1", "1.0.0-alpha"), Ok(-1));
        // 纯数字段按数值。
        assert_eq!(compare_semver("1.0.0-2", "1.0.0-10"), Ok(-1));
    }

    #[test]
    fn compare_semver_empty_errors() {
        // 空串显式报错（Polaris 容忍但产出无意义结果；显式报错防漏更新）。
        assert_eq!(compare_semver("", "1.0.0"), Err(ParseVersionError::Empty));
        assert_eq!(compare_semver("1.0.0", ""), Err(ParseVersionError::Empty));
    }

    #[test]
    fn is_newer_wrapper() {
        assert!(is_newer("1.14.1", "1.14.0").unwrap());
        assert!(!is_newer("1.14.0", "1.14.1").unwrap());
        assert!(!is_newer("1.14.0", "1.14.0").unwrap());
    }
}
