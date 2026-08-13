//! 内核路径解析单点：**可写现役核 + 随包种子**模型（移植 上游 `ResourceManager` 的核路径三腿）。
//!
//! # 为什么要有这个模块（第一性）
//!
//! 换核 = 写内核文件。而 Polaris 的 bundle resources 目录在三平台都**不可靠写**：
//!  - Linux AppImage 挂的是只读 EROFS，根本写不进；
//!  - macOS 写 `.app` bundle 会让二进制脱离原 notarization（上游 靠 ad-hoc 重签兜住，是个 wart）；
//!  - Windows NSIS 重装会用随包种子覆盖用户已更新的核（降级）。
//!
//! 故现役核必须搬到**用户可写目录**，随包核降格为**种子**。这样三平台换核/回滚/重置**全部免提权**
//! （helper 受保护核目录是可选 hardening，不是前置——见 `~/docs/polaris/design/polaris-update-mechanism.md` §7.4）。
//!
//! 对照 上游 `ResourceManager.ts:50-96`（读）/ `:99-120`（写）/ `:345-416`（reseed）。
//! **刻意不复制 上游的 macOS「写回 bundle」腿**：三平台统一指向 `<config_dir>/core_update/`。
//!
//! # 路径布局
//!
//! ```text
//! <config_dir>/                       ← = AppRuntime 的 config_dir（app_config_dir/polaris）
//!   core_update/
//!     sing-box[.exe]                  ← 现役核（可写；resolve_core_binary 的第 2 优先级）
//!     sing-box[.exe].bak              ← 换核前的备份侧车（回滚源）
//!     .core-seed.json                 ← 播种簿记（见 [`CoreSeedMarker`]）
//!   core-staged/                      ← staged 暂存目录（下载完待落位）
//! ```
//!
//! # 为什么用「播种簿记」而不是跑 `<core> version`
//!
//! 上游的版本感知 reseed 靠执行内核二进制读版本。本模块改为把「我们播下去的是什么」记进
//! `.core-seed.json`：
//!  1. **不执行二进制**即可判定是否需要重播种 —— 启动路径零 spawn、零卡顿，且本机开发/CI 可完整单测；
//!  2. 覆盖判定复用既有纯函数 [`decide_core_override`]（official 且旧于随包 → 重播；fork/unknown → **永不覆盖**）；
//!  3. 「有核但无簿记」= 用户自己放进来的 → 判 [`ReseedAction::Keep`]（**绝不覆盖用户的核**，失败安全）；
//!  4. 簿记还带 `source`，据此对「用户手动上传替换」的核做**显式豁免**（[`SOURCE_MANUAL`]）——
//!     「它是什么版本」（`version_line`，如实记）与「能不能替换它」（`source`，尊重意图）由两个
//!     字段各自表达，不让一个空版本串同时兼职两件事。判据全文见 [`decide_reseed`]。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use polaris_updater::core_build::{classify_core_build, ComparableVersion};
use polaris_updater::{decide_core_override, extract_version_token};
use serde::{Deserialize, Serialize};

/// 可写核目录名（= 上游 `userData/core_update/`）。
pub const CORE_UPDATE_DIR_NAME: &str = "core_update";
/// staged 暂存目录名（= 上游 `userData/core-staged/`）。
pub const CORE_STAGED_DIR_NAME: &str = "core-staged";
/// 备份侧车后缀（= 上游 `<core>.bak`）。
pub const CORE_BACKUP_SUFFIX: &str = "bak";
/// 播种簿记文件名。
pub const CORE_SEED_MARKER: &str = ".core-seed.json";

/// 进程级基目录（= `AppRuntime` 的 `config_dir`）。
///
/// `resolve_core_binary()` 是**自由函数**（无 `AppHandle`），故基目录须在启动期一次性注入。
/// 未注入时全部访问器返 `None` → `resolve_core_binary` 回落随包种子（**行为与接线前完全一致**，
/// 单测/子进程等未初始化场景不受影响）。
static CORE_BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 注入基目录（`main.rs` setup 里调一次；重复调用忽略后来者）。
pub fn init_base_dir(config_dir: PathBuf) {
    let _ = CORE_BASE_DIR.set(config_dir);
}

/// 基目录（未注入 → `None`）。
#[must_use]
pub fn base_dir() -> Option<&'static Path> {
    CORE_BASE_DIR.get().map(PathBuf::as_path)
}

// ── 纯路径函数（可注入假 base dir 单测）────────────────────────────────────────

/// 平台核文件名（纯函数：`os` 取 `std::env::consts::OS` 的口径）。
#[must_use]
pub fn core_filename_for(os: &str) -> &'static str {
    if os == "windows" {
        "sing-box.exe"
    } else {
        "sing-box"
    }
}

/// 本平台核文件名。
#[must_use]
pub fn core_filename() -> &'static str {
    core_filename_for(std::env::consts::OS)
}

/// 可写核目录（纯函数）。
#[must_use]
pub fn core_update_dir_in(base: &Path) -> PathBuf {
    base.join(CORE_UPDATE_DIR_NAME)
}

/// 可写现役核路径（纯函数）。
#[must_use]
pub fn writable_core_path_in(base: &Path, os: &str) -> PathBuf {
    core_update_dir_in(base).join(core_filename_for(os))
}

/// staged 暂存目录（纯函数）。
#[must_use]
pub fn staged_dir_in(base: &Path) -> PathBuf {
    base.join(CORE_STAGED_DIR_NAME)
}

/// 备份侧车路径（纯函数）：`<core>` → `<core>.bak`。
///
/// 用 `sing-box.exe` → `sing-box.exe.bak`（**追加**而非替换扩展名）：`Path::with_extension` 会把
/// `.exe` 换掉得到 `sing-box.bak`，与 Unix 侧的 `sing-box.bak` 撞名，跨平台语义不一致。
#[must_use]
pub fn backup_path_for(core: &Path) -> PathBuf {
    let mut s = core.as_os_str().to_os_string();
    s.push(".");
    s.push(CORE_BACKUP_SUFFIX);
    PathBuf::from(s)
}

/// 播种簿记路径（纯函数）。
#[must_use]
pub fn seed_marker_path_in(base: &Path) -> PathBuf {
    core_update_dir_in(base).join(CORE_SEED_MARKER)
}

// ── 运行期访问器（基目录未注入 → None）────────────────────────────────────────

/// 可写核目录（运行期）。
#[must_use]
pub fn core_update_dir() -> Option<PathBuf> {
    base_dir().map(core_update_dir_in)
}

/// 可写现役核路径（运行期）。
#[must_use]
pub fn writable_core_path() -> Option<PathBuf> {
    base_dir().map(|b| writable_core_path_in(b, std::env::consts::OS))
}

/// 备份侧车路径（运行期）。
#[must_use]
pub fn core_backup_path() -> Option<PathBuf> {
    writable_core_path().map(|p| backup_path_for(&p))
}

/// staged 暂存目录（运行期）。
#[must_use]
pub fn staged_dir() -> Option<PathBuf> {
    base_dir().map(staged_dir_in)
}

/// 播种簿记路径（运行期）。
#[must_use]
pub fn seed_marker_path() -> Option<PathBuf> {
    base_dir().map(seed_marker_path_in)
}

/// **随包出厂核**（bundle 种子；绕过可写核优先级）——reset-factory / reseed 的源。
///
/// 单一真值仍在 `proxy.rs`（`resolve_bundled_core_binary`）；本模块只转发，**不复制第二份解析逻辑**
/// （§A3 血证：`GITHUB_HOSTS` 曾有 2 份副本漂移）。
///
/// # Errors
///
/// 随包核不存在（异常打包 / 开发态未跑 `node scripts/fetch-core.mjs`）。
pub fn bundled_core_path() -> Result<PathBuf, String> {
    crate::runtime::proxy::resolve_bundled_core_binary()
}

// ── 播种簿记 + reseed 决策 ───────────────────────────────────────────────────

/// 簿记 `source` = 用户手动上传替换（**唯一参与 reseed 决策的 source 值**，见 [`decide_reseed`]）。
///
/// 常量放在**读侧**（`CoreSeedMarker` 所在处），由写侧 `SwapSource::as_str` 引用 ——
/// 两处各写一份字面量的话，改一处漏一处的症状是**静默的**：写 `"Manual"` 而读比 `"manual"`
/// ⇒ 豁免恒不命中 ⇒ 用户手放的核照旧被随包基线覆盖，而任何单侧单测都还是绿的。
/// 跨边界的端到端用例（`core_swap` 里那条）连同这个常量一起把漂移堵住。
pub const SOURCE_MANUAL: &str = "manual";

/// 播种簿记（`core_update/.core-seed.json`）。
///
/// `version_line` 存的是**原始 `sing-box version` 首行**（含 fork 后缀），因为
/// [`classify_core_build`] 要的正是它 —— 存 token 会丢掉 fork 判据。
/// 播种时我们知道随包基线版本，故无须执行二进制即可写入。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct CoreSeedMarker {
    /// 现役核的原始版本行（我们所知的）；空串 = 未知。
    pub version_line: String,
    /// 写入来源：`bundled` | `update` | `manual` | `reset-factory` | `rollback`。
    ///
    /// **`manual` 参与决策**（[`SOURCE_MANUAL`]，见 [`decide_reseed`]）；其余值仅供诊断。
    pub source: String,
}

impl CoreSeedMarker {
    /// 随包播种的簿记。
    #[must_use]
    pub fn bundled(version: &str) -> Self {
        Self {
            version_line: version.to_string(),
            source: "bundled".to_string(),
        }
    }

    /// 是否为「用户手动上传替换」写下的簿记（reseed 豁免判据）。
    #[must_use]
    pub fn is_manual(&self) -> bool {
        self.source.trim().eq_ignore_ascii_case(SOURCE_MANUAL)
    }
}

/// reseed 决策（纯函数结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReseedAction {
    /// 可写核缺失 → 首次播种（复制随包核）。
    Seed,
    /// 可写核存在但落后于随包基线（且是官方构建）→ 覆盖播种。
    Reseed,
    /// 保持现状（同版 / 更新 / fork / unknown / 无簿记）。
    Keep,
}

/// reseed 决策（**纯函数**，无 IO）：可写核是否需要（重新）从随包核播种。
///
/// 移植 上游 `ensureWritableCore`（`ResourceManager.ts:345-416`）的版本感知语义，但判据从
/// 「执行二进制读版本」换成「读播种簿记」（见模块文档）。
///
/// 真值表：
///  | 可写核存在 | 簿记 | source | 构建 | 版本 vs 随包 | 结果 |
///  |---|---|---|---|---|---|
///  | 否 | — | — | — | — | [`Seed`](ReseedAction::Seed) |
///  | 是 | 无 | — | — | — | [`Keep`](ReseedAction::Keep)（用户自放的核，**绝不覆盖**） |
///  | 是 | 有 | `manual` | 任意 | 任意 | [`Keep`](ReseedAction::Keep)（**显式豁免**，见下） |
///  | 是 | 有 | 其它 | official | 旧 | [`Reseed`](ReseedAction::Reseed) |
///  | 是 | 有 | 其它 | official | 同/新 | [`Keep`](ReseedAction::Keep)（**不降级**用户已更新的核） |
///  | 是 | 有 | 其它 | fork/unknown | 任意 | [`Keep`](ReseedAction::Keep)（**尊重用户的 fork/自建核**） |
///
/// # 为什么 `manual` 要一条显式豁免，而不是靠「版本读不出来」
///
/// 「用户手动上传了这个核」本身就是**显式意图**，它不该靠偶然来受保护。豁免之前的真实形状是：
/// 上传文件名解析得出版本（如 `sing-box-1.13.0-…`）⇒ 写进簿记 ⇒ 判 official 且旧于随包
/// ⇒ **照样被覆盖**；文件名解析不出 ⇒ 簿记空 ⇒ 判 unknown ⇒ 永不覆盖。
/// **同一个用户动作，结果取决于文件名长什么样** —— 那不是失败安全，是不一致。
///
/// 正确的形状是把两件事分开表达，别让一个空串同时兼职：
///  - **簿记如实记版本**（诚实；由 `core_swap::rewrite_marker_from_probe` 按盘上实读补齐）；
///  - **reseed 决策尊重来源**（本函数这条豁免），与下面 fork/unknown 那条腿并列。
///
/// 于是「它是什么」（`classify_core_build`）与「能不能替换它」（source）互不干扰：手动上传的
/// 官方旧核照样被如实记成 official 旧版（诊断/UI 看到的是真话），但不会被随包基线吃掉。
#[must_use]
pub fn decide_reseed(
    writable_exists: bool,
    marker: Option<&CoreSeedMarker>,
    bundled_version: &str,
) -> ReseedAction {
    if !writable_exists {
        return ReseedAction::Seed;
    }
    // 有核但无簿记 = 用户自己放进来的 → 保持（失败安全：宁可不更新，也绝不覆盖用户的核）。
    let Some(m) = marker else {
        return ReseedAction::Keep;
    };
    // 用户手动上传替换的核 → 显式豁免，**与版本/构建判定无关**（理由见函数文档）。
    // 刻意放在 classify/compare 之前：这条豁免不依赖能否解析出版本，正是它存在的意义。
    if m.is_manual() {
        return ReseedAction::Keep;
    }
    let kind = classify_core_build(&m.version_line);
    // ⚠️ `ComparableVersion::normalize` 的正则**锚定行首**（`^v?\d+\.\d+…`），故只吃**版本 token**，
    // 不吃原始版本行。直接喂 `"sing-box version 1.14.0"` 会匹配失败 → 原样返回 → `compare_semver`
    // 报错 → `cmp_against_baseline` 兜成 `-1`（「比基线旧」）→ **任何官方核都会被判 Reseed**，
    // 包括用户刚在线更新到的新核（静默降级回随包版本）。`classify_core_build` 反之要的是原始行
    // （fork 后缀是它的判据）—— 两者入参口径不同，不可混用。
    let token = extract_version_token(&m.version_line);
    let decision =
        decide_core_override(kind, &ComparableVersion::normalize(&token), bundled_version);
    if decision.reseed {
        ReseedAction::Reseed
    } else {
        ReseedAction::Keep
    }
}

/// 读播种簿记（缺失/损坏 → `None`，**不 panic**）。
#[must_use]
pub fn read_seed_marker(base: &Path) -> Option<CoreSeedMarker> {
    let p = seed_marker_path_in(base);
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

/// 写播种簿记（原子 tmp+rename）。
///
/// # Errors
///
/// 序列化 / 落盘失败。
pub fn write_seed_marker(base: &Path, marker: &CoreSeedMarker) -> Result<(), String> {
    let dir = core_update_dir_in(base);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建可写核目录失败 {}: {e}", dir.display()))?;
    let path = seed_marker_path_in(base);
    let json =
        serde_json::to_string_pretty(marker).map_err(|e| format!("序列化播种簿记失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("写播种簿记临时文件失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("原子替换播种簿记失败 {}: {e}", path.display())
    })
}

// ── 落位（薄执行腿）──────────────────────────────────────────────────────────

/// 给核加可执行位（Unix；Windows no-op）。
///
/// # Errors
///
/// chmod 失败。
pub fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(path, perm)
            .map_err(|e| format!("chmod 0755 失败 {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// macOS 落位后处理：清 quarantine + ad-hoc 重签（= 上游 `CoreUpdateService.ts:487-497`）。
///
/// **只在 macOS 编译进来**；其余平台是 no-op。失败**不致命**（仅日志）：Gatekeeper 是否放行属
/// runtime 语义，只能真机验；此处失败即中止落位反而会把「已换好的核」丢在半途。
pub fn post_install_macos(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        // 清扩展属性（下载来的文件带 com.apple.quarantine → Gatekeeper 直接拒绝执行）。
        if let Err(e) = Command::new("/usr/bin/xattr")
            .args(["-cr", &path.to_string_lossy()])
            .status()
        {
            log::warn!("xattr -cr 失败 {e}（换核仍继续；Gatekeeper 行为需真机验）");
        }
        // ad-hoc 重签（`-s -`）：替换进来的二进制无有效签名时 macOS 会杀进程。
        if let Err(e) = Command::new("/usr/bin/codesign")
            .args(["--force", "-s", "-", &path.to_string_lossy()])
            .status()
        {
            log::warn!("codesign --force -s - 失败 {e}（换核仍继续；需真机验）");
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
}

/// 版本感知 reseed（**幂等；失败不 fatal**）。
///
/// 移植 上游 `ensureWritableCore`（`ResourceManager.ts:345-416`）：可写核缺失 → 复制随包核；
/// 落后于随包基线（且官方构建）→ 覆盖。返回**最终应使用的现役核路径**。
///
/// 失败语义（关键）：任何一步失败都返回 `Err`，调用方**照常起核**（`resolve_core_binary` 回落随包
/// 种子）—— 首启/迁移**永不 brick**。
///
/// # Errors
///
/// 随包核不存在 / 复制失败 / chmod 失败。
pub fn ensure_writable_core_at(
    base: &Path,
    os: &str,
    bundled_core: &Path,
    bundled_version: &str,
) -> Result<PathBuf, String> {
    let dest = writable_core_path_in(base, os);
    let marker = read_seed_marker(base);
    let action = decide_reseed(dest.is_file(), marker.as_ref(), bundled_version);
    if action == ReseedAction::Keep {
        return Ok(dest);
    }

    let dir = core_update_dir_in(base);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建可写核目录失败 {}: {e}", dir.display()))?;
    // tmp → rename：与 `verify::atomic_replace` 同款语义（同目录 rename 原子，杜绝半截核）。
    let tmp = dest.with_extension("polaris-seed");
    std::fs::copy(bundled_core, &tmp).map_err(|e| {
        format!(
            "复制随包核失败 {} → {}: {e}",
            bundled_core.display(),
            tmp.display()
        )
    })?;
    make_executable(&tmp)?;
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("原子落位随包核失败 {}: {e}", dest.display())
    })?;
    post_install_macos(&dest);
    write_seed_marker(base, &CoreSeedMarker::bundled(bundled_version))?;
    log::info!(
        "可写现役核已{}：{}（随包基线 {bundled_version}）",
        if action == ReseedAction::Seed {
            "播种"
        } else {
            "重播种"
        },
        dest.display()
    );
    Ok(dest)
}

/// 运行期 reseed（基目录未注入 / 随包核缺失 → `Err`，调用方忽略即可）。
///
/// # Errors
///
/// 基目录未注入 / 随包核不存在 / 落位失败。
pub fn ensure_writable_core(bundled_version: &str) -> Result<PathBuf, String> {
    let base = base_dir().ok_or_else(|| "核基目录未注入（init_base_dir 未调用）".to_string())?;
    let bundled = bundled_core_path()?;
    ensure_writable_core_at(base, std::env::consts::OS, &bundled, bundled_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::runtime::TmpDir;

    fn tmpdir() -> TmpDir {
        TmpDir::new()
    }

    // ── 路径解析纯函数 ──

    #[test]
    fn core_filename_is_platform_specific() {
        assert_eq!(core_filename_for("windows"), "sing-box.exe");
        assert_eq!(core_filename_for("linux"), "sing-box");
        assert_eq!(core_filename_for("macos"), "sing-box");
        // 未知 OS 回落 Unix 名（不 panic、不返空）。
        assert_eq!(core_filename_for("freebsd"), "sing-box");
    }

    #[test]
    fn writable_core_path_layout_is_stable() {
        let base = Path::new("/tmp/polaris-base");
        assert_eq!(
            writable_core_path_in(base, "linux"),
            Path::new("/tmp/polaris-base/core_update/sing-box")
        );
        assert_eq!(
            writable_core_path_in(base, "windows"),
            Path::new("/tmp/polaris-base/core_update/sing-box.exe")
        );
        assert_eq!(
            staged_dir_in(base),
            Path::new("/tmp/polaris-base/core-staged")
        );
        assert_eq!(
            seed_marker_path_in(base),
            Path::new("/tmp/polaris-base/core_update/.core-seed.json")
        );
    }

    #[test]
    fn backup_path_appends_suffix_never_replaces_extension() {
        // 变异防线：若改用 `Path::with_extension("bak")`，Windows 的 sing-box.exe 会得到
        // sing-box.bak —— 与 Unix 侧撞名，跨平台语义漂移。
        assert_eq!(
            backup_path_for(Path::new("/a/core_update/sing-box")),
            Path::new("/a/core_update/sing-box.bak")
        );
        assert_eq!(
            backup_path_for(Path::new("/a/core_update/sing-box.exe")),
            Path::new("/a/core_update/sing-box.exe.bak")
        );
    }

    #[test]
    fn accessors_return_none_when_base_dir_not_injected() {
        // 未注入基目录时全部访问器返 None → resolve_core_binary 回落随包种子（行为与接线前一致）。
        // 注：OnceLock 是进程级；本测试只在 base_dir 未被其它测试注入时有效地断言 None，
        // 故只断言「访问器与 base_dir() 同生共死」这一不变式，不硬断言 None。
        assert_eq!(base_dir().is_none(), writable_core_path().is_none());
        assert_eq!(base_dir().is_none(), core_backup_path().is_none());
        assert_eq!(base_dir().is_none(), staged_dir().is_none());
    }

    // ── reseed 决策真值表 + 逃逸用例（门有没有牙）──

    fn marker(line: &str) -> CoreSeedMarker {
        CoreSeedMarker {
            version_line: line.to_string(),
            source: "test".into(),
        }
    }

    #[test]
    fn reseed_seeds_when_writable_core_missing() {
        assert_eq!(decide_reseed(false, None, "1.13.0"), ReseedAction::Seed);
        // 有簿记但核不在（用户删了）→ 仍须播种。
        assert_eq!(
            decide_reseed(false, Some(&marker("sing-box version 1.13.0")), "1.13.0"),
            ReseedAction::Seed
        );
    }

    #[test]
    fn reseed_keeps_user_placed_core_without_marker() {
        // **逃逸用例**：有核 + 无簿记 = 用户自放。若这里判 Reseed 就会静默吃掉用户的核。
        assert_eq!(decide_reseed(true, None, "9.9.9"), ReseedAction::Keep);
    }

    #[test]
    fn reseed_overwrites_only_older_official_core() {
        // 官方 + 旧 → 重播种（app 升级带来更新的随包核）。
        assert_eq!(
            decide_reseed(true, Some(&marker("sing-box version 1.12.0")), "1.13.0"),
            ReseedAction::Reseed
        );
        // 官方 + 同版 → 保持（不做无谓写盘）。
        assert_eq!(
            decide_reseed(true, Some(&marker("sing-box version 1.13.0")), "1.13.0"),
            ReseedAction::Keep
        );
        // **逃逸用例**：官方 + 更新 → 必须 Keep。判 Reseed 就是把用户已在线更新的核降级回随包版本。
        assert_eq!(
            decide_reseed(true, Some(&marker("sing-box version 1.14.0")), "1.13.0"),
            ReseedAction::Keep
        );
    }

    /// **随包基线跨 prerelease 标识符升级（alpha→beta）必须重播种** —— 端到端串起
    /// 「[`CoreSeedMarker::bundled`] 写的簿记 → [`classify_core_build`] 判官方 →
    /// `compare_semver` 判更旧 → [`ReseedAction::Reseed`]」这条链。
    ///
    /// 为什么非要一条：`bundled()` 写进簿记的是**裸版本 token**（不带 `sing-box version ` 前缀），
    /// 而 `alpha.45`/`beta.3` 的数字段是**降序**（45 → 3）。任一环退化（簿记前缀口径变了 /
    /// prerelease 比较被简化成只比数字段）都会得出「随包核更旧 ⇒ Keep」——症状正是
    /// 「装了新包，运行的还是 `core_update/` 里那个旧核」，而这在盘面上完全无声。
    ///
    /// 变异锁：`crates/updater` 的 `cmp_pre` 改成只比末段数字 → 本用例转红；
    /// `bundled()` 改成写 `format!("sing-box version {v}")` 之外的脏串 → `classify_core_build`
    /// 落 Unknown → Keep → 转红。
    #[test]
    fn reseed_fires_on_bundled_alpha_to_beta_upgrade() {
        assert_eq!(
            decide_reseed(
                true,
                Some(&CoreSeedMarker::bundled("1.14.0-alpha.45")),
                "1.14.0-beta.3"
            ),
            ReseedAction::Reseed
        );
        // 反向（随包核回退到 alpha）→ 绝不降级用户已在跑的 beta。
        assert_eq!(
            decide_reseed(
                true,
                Some(&CoreSeedMarker::bundled("1.14.0-beta.3")),
                "1.14.0-alpha.45"
            ),
            ReseedAction::Keep
        );
    }

    /// **手动上传的核显式豁免 reseed** —— 与「它是什么版本」无关。
    ///
    /// 三行覆盖豁免前会被覆盖的全部形态：官方旧核（文件名解析得出版本的那条老路，**旧行为会
    /// 被吃掉**）、官方新核、版本读不出（空簿记）。豁免只看 `source`，故三者一律 Keep。
    ///
    /// 变异锁：删掉 `decide_reseed` 里的 `if m.is_manual()` 早返回 → 第 1 行转红
    /// （`1.12.0` official 旧于 `1.13.0` ⇒ 落回 Reseed = 用户手放的核被随包基线吃掉）。
    #[test]
    fn reseed_exempts_manually_uploaded_core_regardless_of_version() {
        let manual = |line: &str| CoreSeedMarker {
            version_line: line.to_string(),
            source: SOURCE_MANUAL.to_string(),
        };
        // ① 官方 + 旧于随包 —— 这一行就是裁定要修的形态。
        assert_eq!(
            decide_reseed(true, Some(&manual("sing-box version 1.12.0")), "1.13.0"),
            ReseedAction::Keep,
            "手动上传的核绝不能因为「官方且旧」就被随包基线覆盖"
        );
        // ② 官方 + 更新（本来也 Keep，但要确认豁免没把语义弄反）。
        assert_eq!(
            decide_reseed(true, Some(&manual("sing-box version 1.14.0")), "1.13.0"),
            ReseedAction::Keep
        );
        // ③ 版本读不出（簿记 version_line 空）——豁免不依赖能否解析版本。
        assert_eq!(
            decide_reseed(true, Some(&manual("")), "1.13.0"),
            ReseedAction::Keep
        );
        // 大小写/空白不敏感：簿记是 JSON，手改过的文件不该让豁免静默失效。
        assert_eq!(
            decide_reseed(
                true,
                Some(&CoreSeedMarker {
                    version_line: "sing-box version 1.12.0".into(),
                    source: "  Manual ".into(),
                }),
                "1.13.0"
            ),
            ReseedAction::Keep
        );
    }

    /// 🔴 **豁免的正向对照：非 manual 的官方旧核必须仍被 reseed。**
    ///
    /// 没有这一条，上面那道门可以被「把整条 reseed 关掉」骗过去（`decide_reseed` 无条件返
    /// `Keep` 时它照样绿）—— 而那等于随包核升级对所有人失效，正是本轮一直在修的病。
    ///
    /// 变异锁：把 `is_manual()` 写成恒 `true` / 让 `decide_reseed` 无条件返 `Keep` → 本条转红。
    #[test]
    fn reseed_still_fires_for_non_manual_sources() {
        for source in ["bundled", "update", "rollback", "reset-factory", ""] {
            assert_eq!(
                decide_reseed(
                    true,
                    Some(&CoreSeedMarker {
                        version_line: "sing-box version 1.12.0".into(),
                        source: source.to_string(),
                    }),
                    "1.13.0"
                ),
                ReseedAction::Reseed,
                "source={source:?} 的官方旧核仍必须被随包基线重播种（否则升级对所有人失效）"
            );
        }
    }

    #[test]
    fn reseed_never_overwrites_fork_or_unknown_builds() {
        // **逃逸用例（最要紧）**：fork 核即便版本号旧于随包，也绝不能被覆盖 ——
        // 用户装 fork 是明确选择（reF1nd/nekolsd 之类带特性分支）。
        assert_eq!(
            decide_reseed(
                true,
                Some(&marker("sing-box version 1.11.0-reF1nd")),
                "1.13.0"
            ),
            ReseedAction::Keep
        );
        // unknown（go install / 源码自建 / 探测失败）同样不覆盖。
        assert_eq!(
            decide_reseed(true, Some(&marker("")), "1.13.0"),
            ReseedAction::Keep
        );
        assert_eq!(
            decide_reseed(true, Some(&marker("sing-box version unknown")), "1.13.0"),
            ReseedAction::Keep
        );
    }

    // ── reseed 执行（tempdir 驱动；不碰真 bundle、不起核）──

    #[test]
    fn ensure_writable_core_seeds_then_is_idempotent() {
        let tmp = tmpdir();
        let base = tmp.path();
        // 假随包核（**不用真 bundle**）。
        let bundled = base.join("bundled-sing-box");
        std::fs::write(&bundled, b"BUNDLED-1.13.0").unwrap();

        let dest = ensure_writable_core_at(base, "linux", &bundled, "1.13.0").unwrap();
        assert_eq!(dest, writable_core_path_in(base, "linux"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"BUNDLED-1.13.0");
        assert_eq!(
            read_seed_marker(base).unwrap().version_line,
            "1.13.0",
            "播种后必须写簿记，否则下次启动会判「用户自放」永不重播种"
        );
        // 无 .polaris-seed 残件。
        assert!(!dest.with_extension("polaris-seed").exists());

        // 幂等：同版本再跑不覆盖（把内容改掉验证「确实没重写」）。
        std::fs::write(&dest, b"USER-EDITED").unwrap();
        ensure_writable_core_at(base, "linux", &bundled, "1.13.0").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"USER-EDITED");
    }

    #[test]
    fn ensure_writable_core_reseeds_on_app_upgrade() {
        let tmp = tmpdir();
        let base = tmp.path();
        let bundled = base.join("bundled-sing-box");
        std::fs::write(&bundled, b"OLD").unwrap();
        ensure_writable_core_at(base, "linux", &bundled, "1.12.0").unwrap();

        // 模拟 app 升级：随包核变新。
        std::fs::write(&bundled, b"NEW").unwrap();
        let dest = ensure_writable_core_at(base, "linux", &bundled, "1.13.0").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"NEW");
        assert_eq!(read_seed_marker(base).unwrap().version_line, "1.13.0");
    }

    #[test]
    fn ensure_writable_core_keeps_fork_core_across_app_upgrade() {
        // 端到端逃逸用例：用户换成 fork → app 升级 → fork 必须原样保留。
        let tmp = tmpdir();
        let base = tmp.path();
        let bundled = base.join("bundled-sing-box");
        std::fs::write(&bundled, b"OFFICIAL").unwrap();
        ensure_writable_core_at(base, "linux", &bundled, "1.12.0").unwrap();

        let dest = writable_core_path_in(base, "linux");
        std::fs::write(&dest, b"FORK-BINARY").unwrap();
        write_seed_marker(
            base,
            &CoreSeedMarker {
                version_line: "sing-box version 1.11.0-reF1nd".into(),
                source: "manual".into(),
            },
        )
        .unwrap();

        ensure_writable_core_at(base, "linux", &bundled, "1.13.0").unwrap();
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"FORK-BINARY",
            "app 升级绝不能吃掉用户手动换上的 fork 核"
        );
    }

    #[test]
    fn seed_marker_roundtrip_and_corruption_degrades_to_none() {
        let tmp = tmpdir();
        let base = tmp.path();
        assert!(read_seed_marker(base).is_none());
        write_seed_marker(base, &CoreSeedMarker::bundled("1.13.0")).unwrap();
        assert_eq!(read_seed_marker(base).unwrap().version_line, "1.13.0");
        // 损坏 → None（→ decide_reseed 判 Keep，失败安全，不覆盖用户核）。
        std::fs::write(seed_marker_path_in(base), b"{not json").unwrap();
        assert!(read_seed_marker(base).is_none());
        // 无 .tmp 残件。
        assert!(!seed_marker_path_in(base)
            .with_extension("json.tmp")
            .exists());
    }

    #[test]
    fn ensure_writable_core_fails_loudly_when_bundled_missing() {
        // 失败不 fatal 但必须**如实报错**（调用方据此回落随包种子起核，绝不假成功）。
        let tmp = tmpdir();
        let base = tmp.path();
        let missing = base.join("nope");
        let e = ensure_writable_core_at(base, "linux", &missing, "1.13.0").unwrap_err();
        assert!(e.contains("复制随包核失败"), "实得: {e}");
        assert!(!writable_core_path_in(base, "linux").exists());
    }

    #[cfg(unix)]
    #[test]
    fn seeded_core_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tmpdir();
        let base = tmp.path();
        let bundled = base.join("bundled-sing-box");
        std::fs::write(&bundled, b"X").unwrap();
        // 源文件刻意无执行位 —— 落位必须自己补，否则换核后起核 EACCES。
        std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o644)).unwrap();
        let dest = ensure_writable_core_at(base, "linux", &bundled, "1.13.0").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "落位后的核必须三位可执行，实得 {mode:o}"
        );
    }
}
