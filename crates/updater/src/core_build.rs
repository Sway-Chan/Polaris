//! 内核构建来源判定 + reseed 决策（移植自 上游 `shared/core-build.ts`，§C6 缺口）。
//!
//! 唯一可靠强信号 = `sing-box version` 第一行的版本字符串后缀：fork 刻意在 git tag 打标识
//! （`-reF1nd` / `-nekolsd` / `-nekolsd-test`）。Tags 行不可靠（reF1nd 的 Tags 与官方同构），
//! Revision 离线无法比对。
//!
//! ## 移植来源（逐函数）
//!
//! | Rust | 上游 `core-build.ts` |
//! |---|---|
//! | [`extract_version_token`] | `extractVersionToken:21-29` |
//! | [`parse_uploaded_core_version`] | `parseUploadedCoreVersion:39-47` |
//! | [`ComparableVersion::normalize`] | `comparableCoreVersion:57-60` |
//! | [`classify_core_build`] | `classifyCoreBuild:69-78` |
//! | [`decide_core_override`] | `decideCoreOverride:90-103` |
//! | [`reseed_applied`] | `reseedApplied:112-114` |
//! | [`classify_reseed_result`] | `classifyReseedResult:130-139` |
//!
//! ## 不用 regex 的理由（简约阶梯）
//!
//! Polaris 侧用 5 条正则；本模块**手写解析**而非引入 `regex`：五条正则都是「定长形状校验」
//! （`\d+\.\d+\.\d+` + 固定后缀词表 + hex 段），stdlib 的 `split` / `char::is_ascii_digit` 足够表达，
//! 且本 crate 是纯逻辑 crate（依赖越少编译面越小）。**手写解析的漂移风险由 §C6 金样对拍兜底**：
//! `tests/core_build_golden.rs` 逐条移植 上游 `__tests__/core-build.test.ts` 的全部断言。
//!
//! ## B-2 的结构性防线：[`ComparableVersion`] newtype
//!
//! Polaris 的 `decideCoreOverride` 吃裸 `string`，而它**要求**第 2 参数必须先经 `comparableCoreVersion`
//! 规范化——这个要求只写在注释里，没有任何机制强制。`core-build.ts:53-55` 记录的 B-2 事故正是漏了这步：
//! 完整 dev/fork token（`1.14.0-alpha.31-abcdef1`）喂进 `compareSemver` 时，尾段并入第二 prerelease 段
//! （`"31-abcdef1"` 字母段 > `"37"` 数字段）→ 误判「更新」→ 预判与实际 reseed 反向 → 空头支票。
//!
//! 本移植把该契约**编码进类型**：[`decide_core_override`] 只接受 [`ComparableVersion`]，而它唯一的
//! 构造入口是 [`ComparableVersion::normalize`]（= `comparableCoreVersion`）。裸串调不进来 →
//! B-2 整类 bug 在编译期消失，而非靠调用方记得读注释。

use crate::version::compare_semver;

/// 内核构建来源（= 上游 `CoreBuildKind`，`core-build.ts:12`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreBuildKind {
    /// 官方构建：纯 semver / 官方预发布（`-alpha|beta|rc.N`）/ 官方 dev（base + 7+ 位短 commit hex）。
    Official,
    /// 第三方 fork：以 `X.Y.Z` 开头但带非官方后缀（`-reF1nd` / `-nekolsd` …）。
    Fork,
    /// 无法判定：token 为 `unknown`，或无法解析为 `X.Y.Z` 开头（go install / 源码自建——官方也会
    /// 落到 unknown，**不硬判 fork**）。
    Unknown,
}

/// 剥掉**一个**前导 `v` / `V`（= 上游 `.replace(/^v/i, '')`，只剥一个）。
///
/// 注意与 [`crate::version::compare_semver`] 内部 `trim_start_matches(['v','V'])`（剥多个）的区别：
/// 此处逐字对齐 Polaris 的单次剥除语义。
fn strip_one_leading_v(s: &str) -> &str {
    s.strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s)
}

/// 在 `s` 中查找 `/version\s+(\S+)/i` 的首个匹配，返回捕获组。
///
/// 逐字对齐 JS 正则语义（**非**锚定、**非**单词边界）：从左到右逐个起点尝试，命中
/// 大小写不敏感的 `version` 后要求「≥1 个空白 + ≥1 个非空白」，首个成功的起点获胜。
///
/// 刻意保留 JS 的「子串匹配」怪癖：`"conversion 1.2.3"` 里的 `version` 也算命中（`conVERSION`）。
/// 移植纪律 = 行为逐字一致，不「顺手修正」上游语义。
fn find_token_after_version_keyword(s: &str) -> Option<String> {
    const KEY: &str = "version";
    let lower = s.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(KEY) {
        let after = from + rel + KEY.len();
        let rest = &s[after..];
        // `\s+`：至少一个空白。
        let trimmed = rest.trim_start();
        if trimmed.len() < rest.len() {
            // `(\S+)`：至少一个非空白。
            if let Some(tok) = trimmed.split_whitespace().next() {
                return Some(tok.to_string());
            }
        }
        // 该起点不满足 → 下一个起点继续（对齐正则引擎的回溯尝试）。
        from = from + rel + 1;
    }
    None
}

/// 从 `sing-box version` 第一行（或裸 token）提取版本 token（剥前缀 `v`，到空白为止）。
///
/// 移植自 `core-build.ts:extractVersionToken:21-29`。
#[must_use]
pub fn extract_version_token(version_line: &str) -> String {
    let s = version_line.trim();
    if s.is_empty() {
        return String::new();
    }
    let tok = find_token_after_version_keyword(s)
        .unwrap_or_else(|| s.split_whitespace().next().unwrap_or("").to_string());
    strip_one_leading_v(&tok).trim().to_string()
}

/// 手动上传核的版本解析结果（= 上游 `parseUploadedCoreVersion` 的返回对象）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedCoreVersion {
    /// 完整版本 token（保留 prerelease + fork 后缀），经 `^\d+\.\d+` 形状校验；
    /// 不符（纯数字 int / 脏行 / 空）→ [`None`]，**绝不谎报**。
    pub version: Option<String>,
    /// stdout 第一行原始串（含 fork 后缀），供 [`classify_core_build`] 判来源。
    pub version_line: String,
}

/// `^\d+\.\d+` 形状校验（= 上游 `/^\d+\.\d+/.test(token)`）：至少「数字段 . 数字段」开头。
fn has_major_minor_shape(tok: &str) -> bool {
    let mut it = tok.splitn(2, '.');
    let (Some(major), Some(rest)) = (it.next(), it.next()) else {
        return false;
    };
    // major 全数字且非空；rest 以数字开头。
    !major.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && rest.starts_with(|c: char| c.is_ascii_digit())
}

/// 解析 `sing-box version` 完整输出为 [`UploadedCoreVersion`]（手动上传 / 预检专用）。
///
/// 移植自 `core-build.ts:parseUploadedCoreVersion:39-47`。只取 stdout 第一行——版本恒在首行，
/// 避免 `Environment: go1.25.10` 行的 `go1.25.10` 被误匹配。
#[must_use]
pub fn parse_uploaded_core_version(stdout: &str) -> UploadedCoreVersion {
    let version_line = stdout.split('\n').next().unwrap_or("").trim().to_string();
    let token = extract_version_token(&version_line);
    let version = has_major_minor_shape(&token).then_some(token);
    UploadedCoreVersion {
        version,
        version_line,
    }
}

/// 「可比较形」版本 —— [`decide_core_override`] 的唯一入参类型（B-2 的结构性防线，见模块文档）。
///
/// 内含值恒为 [`ComparableVersion::normalize`] 的产物：`major.minor[.patch][-单段后缀]`，
/// **fork/dev 尾段（第二个 `-` 起）已截断**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparableVersion(String);

impl ComparableVersion {
    /// 把版本 token 规范化为「可比较形」：保留 `major.minor[.patch][-prerelease.N]`，
    /// **截断 fork/dev 尾段（第二个 `-` 起）**。
    ///
    /// 移植自 `core-build.ts:comparableCoreVersion:57-60`
    /// （正则 `/^v?(\d+\.\d+(?:\.\d+)?(?:-[0-9A-Za-z.]+)?)/i`）。
    ///
    /// 与 上游 `ProxyManager.getCoreVersion` 落到 `coreVersion` 的截断口径**同形**——
    /// 使「手动上传基线预判」与「启动 reseed 决策」喂进 [`compare_semver`] 的 token 一致。
    /// 非版本串（纯 int / 脏行 / 空）**原样返回**（对齐 Polaris：unknown 核不参与 reseed 决策，无害）。
    #[must_use]
    pub fn normalize(version: &str) -> Self {
        Self(normalize_comparable(version))
    }

    /// 取内含的可比较版本串。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ComparableVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `comparableCoreVersion` 的解析本体（手写 `/^v?(\d+\.\d+(?:\.\d+)?(?:-[0-9A-Za-z.]+)?)/i`）。
///
/// 逐段推进：可选 `v` → 数字段 → `.` → 数字段 → 可选(`.` 数字段) → 可选(`-` + `[0-9A-Za-z.]+`)。
/// 任一必需段缺失 → 无匹配 → 原样返回（对齐正则 `m ? m[1] : version || ''`）。
fn normalize_comparable(version: &str) -> String {
    let src = version;
    let body = strip_one_leading_v(src);
    let b = body.as_bytes();
    let mut i = 0usize;

    // `\d+`（major）
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return src.to_string();
    }
    // `\.`
    if i >= b.len() || b[i] != b'.' {
        return src.to_string();
    }
    i += 1;
    // `\d+`（minor）
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return src.to_string();
    }
    // `(?:\.\d+)?`（patch，可选——须 `.` 后确有数字才消费）
    if i < b.len() && b[i] == b'.' {
        let mut j = i + 1;
        let start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            i = j;
        }
    }
    // `(?:-[0-9A-Za-z.]+)?`（单段后缀，可选——字符类**不含 `-`**，故在第二个 `-` 处自然停）
    if i < b.len() && b[i] == b'-' {
        let mut j = i + 1;
        let start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.') {
            j += 1;
        }
        if j > start {
            i = j;
        }
    }
    body[..i].to_string()
}

/// `^\d+\.\d+\.\d+` 前缀校验 + 返回其后的剩余串（供官方形态判定复用）。
///
/// 返回 `Some(rest)` = token 以三段数字版本开头，`rest` 为其后的剩余（可能为空）。
fn split_triple_numeric_prefix(tok: &str) -> Option<&str> {
    let b = tok.as_bytes();
    let mut i = 0usize;
    for seg in 0..3 {
        if seg > 0 {
            if i >= b.len() || b[i] != b'.' {
                return None;
            }
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    Some(&tok[i..])
}

/// `-(alpha|beta|rc)\.\d+` 校验：返回其后的剩余串。
fn split_official_prerelease(rest: &str) -> Option<&str> {
    for tag in ["-alpha.", "-beta.", "-rc."] {
        if let Some(after) = rest.strip_prefix(tag) {
            let digits: usize = after.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                return Some(&after[digits..]);
            }
        }
    }
    None
}

/// `-[0-9a-fA-F]{7,}$` 校验（官方 dev 的短 commit hex 尾段；大小写不敏感）。
fn is_official_dev_hex_tail(rest: &str) -> bool {
    let Some(hex) = rest.strip_prefix('-') else {
        return false;
    };
    hex.len() >= 7 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 判定内核构建来源。
///
/// 移植自 `core-build.ts:classifyCoreBuild:69-78`：
///  - [`CoreBuildKind::Official`]：纯 semver / 官方预发布（`-alpha|beta|rc.N`）/ 官方 dev
///    （base + `-` + 7+ 位 hex 短 commit）。手动上传的官方跨版本（任意 `X.Y.Z`）零误报——
///    规则不依赖具体版本号。
///  - [`CoreBuildKind::Unknown`]：token 为 `unknown`，或无法解析为 `X.Y.Z` 开头
///    （go install / 源码自建——官方也会 unknown，**不硬判 fork**）。
///  - [`CoreBuildKind::Fork`]：以 `X.Y.Z` 开头但带非官方后缀（含非 hex 字母词，如 `-reF1nd` / `-nekolsd`）。
#[must_use]
pub fn classify_core_build(version_line: &str) -> CoreBuildKind {
    let tok = extract_version_token(version_line);
    if tok.is_empty() || tok.eq_ignore_ascii_case("unknown") {
        return CoreBuildKind::Unknown;
    }
    // 官方三形态（= OFFICIAL_RELEASE / OFFICIAL_PRERELEASE / OFFICIAL_DEV 三条正则的并集）。
    if let Some(rest) = split_triple_numeric_prefix(&tok) {
        // `^\d+\.\d+\.\d+$` —— 纯 release。
        if rest.is_empty() {
            return CoreBuildKind::Official;
        }
        // `^\d+\.\d+\.\d+-(alpha|beta|rc)\.\d+$` —— 官方预发布。
        if let Some(after_pre) = split_official_prerelease(rest) {
            if after_pre.is_empty() {
                return CoreBuildKind::Official;
            }
            // `^…(-(alpha|beta|rc)\.\d+)?-[0-9a-fA-F]{7,}$` —— 预发布 + dev hex 尾。
            if is_official_dev_hex_tail(after_pre) {
                return CoreBuildKind::Official;
            }
        }
        // `^\d+\.\d+\.\d+-[0-9a-fA-F]{7,}$` —— release + dev hex 尾。
        if is_official_dev_hex_tail(rest) {
            return CoreBuildKind::Official;
        }
        // 以 X.Y.Z 开头但后缀非官方形态 → fork。
        return CoreBuildKind::Fork;
    }
    // 非 X.Y.Z 开头 = 脏输入/无法解析 → unknown（**不误判 fork**）。
    CoreBuildKind::Unknown
}

/// 核覆盖决策结果（= 上游 `decideCoreOverride` 的返回对象 `{ reseed, warn }`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreOverrideDecision {
    /// 是否用随包（内置）核替换本机在用核。**本仓真正消费的就是这一个字段。**
    pub reseed: bool,
    /// 是否发「基线兼容风险」提醒（fork/unknown 且 ≤ 基线）。
    ///
    /// 🔴 **本仓生产侧不消费本字段，且不要把它接上去** —— 提醒那条腿的权威在
    /// `src-tauri/src/runtime/startup_tasks.rs::should_warn_core_baseline`（#17，6s 启动任务）。
    ///
    /// 两者**故意不同构**，接错会引入误报：本字段经 [`cmp_against_baseline`]，把
    /// `compare_semver` 的解析失败折成 `-1`（「比基线旧」）以**与 上游 逐字一致**
    /// ⇒ 版本串读不出来时 `warn == true`；而 `should_warn_core_baseline` 先用
    /// `looks_like_version` 拦下不可解析输入、判**不提醒**（「宁可漏报不误报」）。
    /// 那是 Polaris 侧有意的修正，不是遗漏。
    ///
    /// 本字段保留的理由是**对照件价值**：`tests/core_build_golden.rs` 拿它与 上游
    /// `core-build.ts:decideCoreOverride` 逐格对拍（含一条上游套件从未覆盖的 `==` 用例，
    /// 见 `decide_fork_equal_to_bundled_warns_polaris_addition`）。删掉它等于把那套对拍拆一半。
    pub warn: bool,
}

/// 比较「可比较形」版本与基线；空串等价 `0.0.0`（对齐 上游 `compareSemver` 对空串的容忍口径）。
///
/// Polaris 的 `compareSemver('')` 走 `parseInt('')||0` → 主段 `[0]`，即空串 ≈ `0.0.0`（任何真实基线都更大
/// → 返回 -1）。本 crate 的 [`compare_semver`] 对空串**显式报错**（设计上更严）；此处在 core-build 边界
/// 把该错误折回 Polaris 语义（`Less`），使决策与上游逐字一致，且失败安全：
/// unknown + 空版本 → `warn=true, reseed=false`（绝不覆盖用户的核）。
fn cmp_against_baseline(version: &str, bundled: &str) -> i32 {
    compare_semver(version, bundled).unwrap_or(-1)
}

/// 核覆盖决策（纯函数）：启动时是否用随包（内置）核替换本机正在使用的**非内置**核。
///
/// 移植自 `core-build.ts:decideCoreOverride:90-103`。内置核本身是种子（强制落位），不经此决策；
/// 此处只判用户单独装的核。基线 = 随包（内置）版本。
///
///  - `official` + 本机 < 内置 → `reseed`（统一到随包基线）
///  - `official` + 本机 ≥ 内置 → `keep`（同版不重播种；更新不降级用户装的官方核）
///  - `fork` / `unknown` → `keep`（**绝不覆盖用户的 fork/自建核**）；本机 ≤ 内置 → `warn`（兼容提醒）
///
/// # 为什么第 2 参数是 [`ComparableVersion`] 而非 `&str`
///
/// 见模块文档「B-2 的结构性防线」：裸串会让 dev/fork 完整尾段污染 [`compare_semver`]，
/// 上游只能靠注释约束调用方。此处由类型强制。
#[must_use]
pub fn decide_core_override(
    kind: CoreBuildKind,
    core_version: &ComparableVersion,
    bundled_version: &str,
) -> CoreOverrideDecision {
    let cmp = cmp_against_baseline(core_version.as_str(), bundled_version);
    if kind == CoreBuildKind::Official {
        // 官方非内置核：严格旧于内置 → 内置替换；同版/更新 → 保持。
        return CoreOverrideDecision {
            reseed: cmp < 0,
            warn: false,
        };
    }
    // fork / unknown：尊重用户选择，绝不覆盖；≤ 基线时提醒兼容风险（含同版 fork——后缀分支可能缺新特性）。
    CoreOverrideDecision {
        reseed: false,
        warn: cmp <= 0,
    }
}

/// reseed（随包核替换）是否「真的」生效（纯函数）：换核后重读活二进制版本 ≥ 随包基线即判生效。
///
/// 移植自 `core-build.ts:reseedApplied:112-114`。旧核被运行中 sing-box 进程占用（Linux `ETXTBSY`）
/// / 写盘失败时版本不变（仍 < 基线）→ 判未生效，调用方据此**诚实上报失败**、绝不谎报「刷新成功」
/// （Polaris issue #150）。
#[must_use]
pub fn reseed_applied(version_after: &str, bundled_version: &str) -> bool {
    cmp_against_baseline(version_after, bundled_version) >= 0
}

/// reseed 结果判定（= 上游 `classifyReseedResult` 的返回对象）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReseedResult {
    /// 应记录的「当前版本」。
    pub version: String,
    /// reseed 是否真生效。
    pub applied: bool,
}

/// 判定 reseed 结果（纯函数，移植自 `core-build.ts:classifyReseedResult:130-139`）。
///
/// # ⚠️ 调用方契约（Polaris issue #150 review F1）
///
/// `line_after` **必须**来自「探测失败置空」的读法（失败返回 `""`），**绝不**用「探测失败会回落随包基线」
/// 的读法——否则「重读 spawn 失败」会被回落值伪装成「换核成功」，令版本闸门误放行、带旧核硬跑退回死循环。
///
/// 本仓对应关系（`runtime/updater.rs`）：
///  - ✅ `read_core_version_line()`：探测失败 → `""`（**本函数唯一合法的入参来源**）
///  - ❌ `read_core_version()`：探测失败 → 回落随包基线（**绝不可喂给本函数**）
///
/// 语义：
///  - `line_after` 解析不出版本（探测失败/脏行）→ 保守保留换核前旧版本 `before`、判未生效（诚实失败）。
///  - 解析出版本 → 该版本即当前版本，是否生效 = [`reseed_applied`]。
#[must_use]
pub fn classify_reseed_result(
    line_after: &str,
    before: &str,
    bundled_version: &str,
) -> ReseedResult {
    let version_after = extract_version_token(line_after);
    // 须解析出形如 `X.…` 的真实版本 token；空（探测失败）/ 非数字开头（脏行，如裸 `sing-box`）
    // → 保守保留旧版本、判未生效。
    if !version_after.starts_with(|c: char| c.is_ascii_digit()) {
        return ReseedResult {
            version: before.to_string(),
            applied: false,
        };
    }
    let applied = reseed_applied(&version_after, bundled_version);
    ReseedResult {
        version: version_after,
        applied,
    }
}
