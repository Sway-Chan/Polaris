//! GitHub release JSON → 更新检查结果的**纯逻辑**转换 + 平台/架构资产选择 + 更新源常量。
//!
//! ## 移植来源（上游 TS → Rust 纯逻辑，逐字保留行为）
//!
//! - `UpdateService.fetchReleases`（`UpdateService.ts:843`）+ `UpdateService.checkForUpdate`
//!   （`:146-240`）：拉 `api.github.com/repos/{owner}/{repo}/releases` → 过滤 prerelease → 按
//!   `published_at` 降序取最新 → 去 `v` 前缀 → `compareSemver` 判新 → `findSuitableAsset` 挑平台包
//!   → 组装 `UpdateInfo`。本模块把**除网络获取以外**的全部逻辑抽成纯函数 [`check_app_update`]
//!   （吃已拉回的 JSON 字节，宿主注入 HTTP），可 mock 单测。
//! - `update-asset.findSuitableUpdateAsset`（`update-asset.ts`，52 行）：App 安装包资产选择
//!   （每平台 loose/installed 双形态消歧，#72）→ [`find_suitable_update_asset`]。
//! - `singbox-asset.findSuitableSingboxAsset`（`singbox-asset.ts`，62 行）：内核资产选择
//!   （平台/架构关键词 + with-naive/full 优先）→ [`find_suitable_singbox_asset`]。
//! - 更新源仓库常量（上游 `UpdateService.ts:43-44` `GITHUB_OWNER/GITHUB_REPO`；
//!   `core-downloader.ts:69` `SagerNet/sing-box`）→ [`APP_UPDATE_REPO`] / [`CORE_UPDATE_REPO`]。
//!
//! ## 为什么平台/架构是**参数**而非读 `process`
//!
//! 与 上游的 `findSuitable*` 同纪律：平台/架构由调用方注入（上游 传 `process.platform/arch`，
//! 本仓宿主传 `std::env::consts::OS/ARCH` 经 [`AssetPlatform::from_os`] / [`AssetArch::from_arch`]
//! 映射）。如此本函数**不读全局态**，可用平台真值表全覆盖单测。这也是 `manifest.rs` 早先
//! `AssetSelector` trait 的意图落地版——具体规则移进本模块（纯函数，参数注入），不再需要宿主注入 trait。

use serde::{Deserialize, Serialize};

use crate::manifest::ManifestError;
use crate::version::compare_semver;

// ── 更新源仓库常量（单点定义，宿主不得自造第二份）────────────────────────────────

/// App 自更新源仓库 `(owner, repo)`（= 上游 `GITHUB_OWNER/GITHUB_REPO` 的 Polaris 对应）。
pub const APP_UPDATE_REPO: (&str, &str) = ("Sway-Chan", "polaris");

/// 内核（sing-box）更新源仓库 `(owner, repo)`（= 上游 `core-downloader.ts` 的 `SagerNet/sing-box`）。
pub const CORE_UPDATE_REPO: (&str, &str) = ("SagerNet", "sing-box");

/// Windows 便携版 release 资产的文件名前缀（完整形态 `polaris-portable-<label>.zip`）。
///
/// **跨文件命名契约的单点定义**，三处必须一致，改一处就要改全部：
///  1. 产出侧 `.github/workflows/package.yml` 的 `Build Windows portable zip` 步；
///  2. 选包侧 [`find_suitable_update_asset`] 的 Windows loose 分支（本模块）；
///  3. 断言侧 `scripts/verify-packaging.mjs` 的 `updaterPortableCandidates`。
///
/// 大小写敏感：`package.yml` 产的是字面小写名，三侧同口径才守得住真正会被选中的那个资产。
pub const PORTABLE_ZIP_PREFIX: &str = "polaris-portable-";

/// 构造 GitHub releases API URL（= 上游 `https://api.github.com/repos/${owner}/${repo}/releases`）。
#[must_use]
pub fn github_releases_api_url(owner: &str, repo: &str) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/releases")
}

// ── 目标平台 / 架构（对齐 NodeJS.Platform / process.arch 的分支面）──────────────────

/// 目标平台（对齐 上游 `process.platform` 的三分支 `win32`/`darwin`/`linux`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetPlatform {
    Windows,
    Macos,
    Linux,
}

/// 目标架构（对齐 上游 `process.arch` 关心的 `x64`/`arm64`；其余归 [`Other`](AssetArch::Other)）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetArch {
    X64,
    Arm64,
    Other,
}

impl AssetPlatform {
    /// 从 `std::env::consts::OS` 映射（宿主注入真实平台）。`None` = 非三大目标平台（无适配包）。
    #[must_use]
    pub fn from_os(os: &str) -> Option<Self> {
        match os {
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::Macos),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }
}

impl AssetArch {
    /// 从 `std::env::consts::ARCH` 映射（`x86_64` → X64、`aarch64` → Arm64，其余 → Other）。
    #[must_use]
    pub fn from_arch(arch: &str) -> Self {
        match arch {
            "x86_64" => Self::X64,
            "aarch64" => Self::Arm64,
            _ => Self::Other,
        }
    }
}

// ── GitHub release JSON 形状（只取本模块需要的字段）───────────────────────────────

/// GitHub release 资产（`assets[]` 元素）。
#[derive(Debug, Clone, Deserialize)]
pub struct GithubAsset {
    /// 资产文件名（平台/架构/形态的判据来源）。
    pub name: String,
    /// 直下 URL（= 上游 `asset.browser_download_url`）。
    #[serde(rename = "browser_download_url", default)]
    pub browser_download_url: String,
    /// 字节大小（= 上游 `asset.size`；缺失按 0）。
    #[serde(default)]
    pub size: u64,
    /// GitHub 给出的内容摘要，形如 `sha256:ab12…`（较新的 REST API 字段；旧 release 可能缺失）。
    ///
    /// **供应链增强（上游 没有这一层）**：下载件按它做 sha256 强校验，补上「HTTPS 只保传输、
    /// 镜像回退把信任面扩到 gh-proxy 运营方」的洞。信任根仍是 GitHub（摘要与资产同源），
    /// 故**不需要自建密钥**——防的是截断/镜像投毒，不是防 GitHub 本身。
    /// 缺失时回落 Content-Length 完整性校验（= 上游 基线），**不因缺摘要就拒绝更新**。
    #[serde(default)]
    pub digest: Option<String>,
}

/// GitHub release（`/releases` 数组元素）。
#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    /// 版本 tag（如 `v0.2.0`；= 上游 `tag_name`）。
    pub tag_name: String,
    /// release 标题（= 上游 `name`；缺省回落 tag）。
    #[serde(default)]
    pub name: Option<String>,
    /// 发布说明正文（= 上游 `body`）。
    #[serde(default)]
    pub body: Option<String>,
    /// 是否预览版（= 上游 `prerelease`）。
    #[serde(default)]
    pub prerelease: bool,
    /// 发布时间（RFC3339 UTC，如 `2024-05-01T12:00:00Z`；= 上游 `published_at`）。
    #[serde(default)]
    pub published_at: Option<String>,
    /// release 资产列表（= 上游 `assets`）。
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

// ── App 自更新检查结果（移植 UpdateService 的 UpdateInfo / UpdateCheckResult）──────────

/// App 更新信息（= 上游 `UpdateInfo`；字段名 camelCase，与 `ui` 的 `UpdateInfo` 契约逐字对齐）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInfo {
    /// 原始 tag（**含 `v`**，= 上游 `UpdateInfo.version = latestRelease.tag_name`）。
    pub version: String,
    /// 标题（= 上游 `name || tag_name`）。
    pub title: String,
    /// 发布说明（= 上游 `body || ''`）。
    pub release_notes: String,
    /// 下载 URL（选中资产的 `browser_download_url`）。
    pub download_url: String,
    /// 文件大小（选中资产的 `size`）。
    pub file_size: u64,
    /// 发布时间。
    pub published_at: String,
    /// 是否预览版。
    pub is_prerelease: bool,
    /// 资产文件名。
    pub file_name: String,
    /// 选中资产的期望 sha256（由 GitHub `digest` 字段解析；旧 release 无该字段 → `None`）。
    ///
    /// 下载侧据此做强校验（**上游 没有这一层**）。`None` 时回落 Content-Length 完整性校验，
    /// **不因缺摘要就拒绝更新**——否则所有旧 release 都会更新不了。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// App 更新检查结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppUpdateCheck {
    /// 无可用更新（已最新 / 无正式版 / 已跳过此版本 / 无适配平台资产）。
    NoUpdate,
    /// 有可用更新。
    Available(AppUpdateInfo),
}

/// GitHub release asset 的 `digest` 字段 → 裸 sha256 hex（**纯函数**）。
///
/// GitHub 返回形如 `sha256:ab12…`。非 sha256 算法（未来若加 blake3 之类）一律返 `None`
/// —— **绝不把不认识的摘要当 sha256 喂进 [`verify_bytes`](crate::verify::verify_bytes)**：
/// 那会必然 mismatch，把「本地不支持该摘要算法」伪装成「下载件被篡改」，成因错位。
#[must_use]
pub fn parse_asset_digest(digest: &str) -> Option<String> {
    let hex = digest.trim().strip_prefix("sha256:")?;
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

/// 去 tag 的前导 `v`（= 上游 `tag_name.replace(/^v/, '')`——仅小写 `v`，仅前导一处）。
///
/// `pub` 不是给外部自由用：[`check_app_update`] 拿它算比较侧（`strip_v(tag)`），而「跳过此版本」
/// 的**存储侧**必须用同一个函数归一化（W8）——两侧各自实现一份就是「v0.2.0 存进去、0.2.0 比
/// 出来，永不相等」的原发病理。导出它是为了让写点复用同一真值。
pub fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// 挑更新目标 release：过滤 prerelease 后按 `published_at` **降序取最新**
/// （= 上游 `validReleases.sort((a,b) => dateB - dateA)[0]`）。
///
/// `published_at` 为 RFC3339 UTC（固定宽度、恒带 `Z`），字典序 == 时间序，故直接比字符串等价于
/// 上游的 `new Date(...).getTime()` 比较，且零依赖、无需日期解析。缺失 `published_at`（草稿等）
/// 按空串处理（排到最旧，仅当它是唯一候选才被选中——对齐 上游 `new Date(null)=epoch0` 沉底）。
fn select_update_release(
    releases: &[GithubRelease],
    include_prerelease: bool,
) -> Option<&GithubRelease> {
    releases
        .iter()
        .filter(|r| include_prerelease || !r.prerelease)
        .max_by(|a, b| {
            a.published_at
                .as_deref()
                .unwrap_or("")
                .cmp(b.published_at.as_deref().unwrap_or(""))
        })
}

/// 检查 App 更新（移植 `UpdateService.checkForUpdate` 的纯逻辑部分）。
///
/// 步骤（逐字对齐 `UpdateService.ts:151-212`）：
///  1. 解析 releases JSON（网络获取由宿主注入，本函数只吃已拉回的字节 → 纯逻辑可单测）。
///  2. 过滤 prerelease + 按 `published_at` 降序取最新（[`select_update_release`]）。
///  3. 去 `v` 前缀 → `compare_semver > 0` 判新（空/不可解析按**无更新**处理，失败安全）。
///  4. 命中用户「跳过此版本」→ 无更新。
///  5. [`find_suitable_update_asset`] 挑平台/架构/形态资产；无适配 → 无更新。
///  6. 组装 [`AppUpdateInfo`]（`version` 保留原始 tag，对齐 上游）。
///
/// # Errors
///
/// - [`ManifestError::ParseJson`]：releases JSON 解析失败（= 上游 `解析 GitHub API 响应失败`）。
pub fn check_app_update(
    releases_json: &str,
    current_version: &str,
    include_prerelease: bool,
    skipped_version: Option<&str>,
    platform: AssetPlatform,
    arch: AssetArch,
    loose_form: bool,
) -> Result<AppUpdateCheck, ManifestError> {
    let releases: Vec<GithubRelease> =
        serde_json::from_str(releases_json).map_err(|e| ManifestError::ParseJson(e.to_string()))?;

    let Some(release) = select_update_release(&releases, include_prerelease) else {
        // 无（正式）发布版本（= 上游 `未找到发布版本`）。
        return Ok(AppUpdateCheck::NoUpdate);
    };

    let latest_version = strip_v(&release.tag_name);

    // 必须比 current 新（compare_semver 对空串/不可解析报错 → 失败安全按无更新，绝不误报有更新）。
    let is_newer = compare_semver(latest_version, current_version)
        .map(|ord| ord > 0)
        .unwrap_or(false);
    if !is_newer {
        return Ok(AppUpdateCheck::NoUpdate);
    }

    // 用户已跳过此版本（= 上游 `if (this.skippedVersion === latestVersion)`）。
    if skipped_version == Some(latest_version) {
        return Ok(AppUpdateCheck::NoUpdate);
    }

    let Some(asset) = find_suitable_update_asset(&release.assets, platform, arch, loose_form)
    else {
        // 无适配当前平台的安装包（= 上游 `未找到适合当前平台的安装包`）。
        return Ok(AppUpdateCheck::NoUpdate);
    };

    Ok(AppUpdateCheck::Available(AppUpdateInfo {
        version: release.tag_name.clone(),
        title: release
            .name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| release.tag_name.clone()),
        release_notes: release.body.clone().unwrap_or_default(),
        download_url: asset.browser_download_url.clone(),
        file_size: asset.size,
        published_at: release.published_at.clone().unwrap_or_default(),
        is_prerelease: release.prerelease,
        file_name: asset.name.clone(),
        sha256: asset.digest.as_deref().and_then(parse_asset_digest),
    }))
}

// ── App 安装包资产选择（移植 update-asset.findSuitableUpdateAsset，逐字保留）──────────

/// 从 release 资产里挑适配 `(platform, arch, loose_form)` 的 **App 安装包**。
///
/// 移植自 `update-asset.findSuitableUpdateAsset`（#72：每平台 loose/installed 双形态须按**当前运行
/// 形态**选对应包，否则错配——便携被发 NSIS setup 会装出多余副本）：
///  - Windows：**两条互不相交的规则**（见下）——loose→`polaris-portable-*.zip`（无则 `None`）/
///    installed→`.exe` 且名含 `win`（setup → 非 portable → 首个）。
///  - macOS：按架构 `mac-arm64`/`mac-x64` 的 `.dmg`；**无则 `None`，不回落任意 `.dmg`**
///    （分架构单出后回落 = 发错架构包，见 macOS 分支注释；`.app` 恒 loose，不分形态）。
///  - Linux：loose→`.AppImage`（无则 `.deb`）/ installed→`.deb`（无则 `.AppImage`）。
///
/// ## Windows 为什么按形态分成两条**独立**规则（2026-07-22 修 #72 形态错配本体）
///
/// 本仓 Windows 的两件交付物分属**两个不相交的命名空间**（核对于 `package.yml` 的实际产物名）：
///
/// | 形态 | 产物 | 谁选它 |
/// |---|---|---|
/// | installed | `*-win-setup.exe`（NSIS downloadBootstrapper） | `.exe` 且名含 `win` |
/// | loose | `polaris-portable-*.zip`（`Compress-Archive` 打的免安装 zip） | `polaris-portable-` 前缀 + `.zip` |
///
/// 便携产物是 **zip**，结构性进不了「`.exe` 且名含 `win`」这道过滤。此前 loose 分支与 installed
/// 分支**共用**那个候选集，于是 `find(is_portable)` 空 → `find(!is_setup)` 空 → `first()`
/// **无条件命中 NSIS setup**：便携用户被发安装器，装出与便携副本并存的第二份程序
/// （配置目录 / 自启项 / 内核资源各一套）—— 正是本函数头部自称要防的 #72 形态错配本体。
///
/// 现改为 loose 走自己的规则，且**无回落**：选不到即 `None`（= 无更新）。理由与 macOS 那条同源
/// ——**宁可不更新，也不发错形态包**。回落到安装器不是「降级但可用」，而是制造第二份安装。
///
/// 两条规则的判据不相交（`.zip` vs `.exe`），故各自无歧义，`.exe && contains("win")`
/// 这条命名契约**一字未动**。
///
/// ## 为什么 Windows / Linux 的 installed 侧仍保留回落（别按对称性「顺手修」）
///
/// macOS 取消回落跨的是**架构**：release 缺一份 dmg 时把 arm64 包发给 Intel 用户 = 装了也跑不起来。
/// Windows / Linux 在本仓 CI matrix 里各只有**一个** target（`x86_64-pc-windows-msvc` /
/// `x86_64-unknown-linux-gnu`），资产名里根本没有架构判别位 ⇒ 回落绝不会发错架构。
/// 将来 Windows/Linux 真出 arm64 包时，本结论失效，须回来按 macOS 同法处理。
///
/// Linux 的 loose↔installed 回落**保留**：`.deb` 与 `.AppImage` 同 release 都在（`package.yml` 的
/// assets 断言机守着两者皆存），两者都是**单文件安装件**、下游 `decide_install_plan` 认得，
/// 回落跨形态是降级但可用，正是 #72 要保的 上游 行为。Windows 的 zip↔exe 不同：zip 不是安装件，
/// 反向回落（安装态拿 zip）无意义，正向回落（便携拿 exe）就是缺陷本身，故两侧都不回落。
///
/// ## 便携更新的下游：**交系统，不自动替换**（如实登记，别读成全自动）
///
/// 选中的 `.zip` 走到 `runtime::update_install::classify_installer` 时**不被识别**
/// （它只认 `.exe/.dmg/.appimage/.deb`）⇒ `InstallReject::UnknownAsset` ⇒ command 层回退
/// `shell.open` 打开该 zip，由用户自行解压覆盖。这是**有意的诚实降级**：便携用户拿到的是
/// 正确形态的产物，且绝不会有 NSIS 在背后装出第二份。自动解压替换需引 zip 解压依赖，未做。
#[must_use]
pub fn find_suitable_update_asset(
    assets: &[GithubAsset],
    platform: AssetPlatform,
    arch: AssetArch,
    loose_form: bool,
) -> Option<&GithubAsset> {
    match platform {
        AssetPlatform::Windows => {
            if loose_form {
                // 便携(loose)：**独立规则、无回落**。判据与 `verify-packaging.mjs` 的
                // `updaterPortableCandidates` 逐字同口径（大小写敏感，同 `package.yml` 里
                // `polaris-portable-${BUILD_LABEL}.zip` 的字面产物名）——两侧判据一旦不一致，
                // 那道断言守的就不是选包器真正会选的东西。
                return assets
                    .iter()
                    .find(|a| a.name.starts_with(PORTABLE_ZIP_PREFIX) && a.name.ends_with(".zip"));
            }
            // 安装态口径：`.exe` 且名含 'win'（大小写敏感，与 上游 一致）。
            let win_exe: Vec<&GithubAsset> = assets
                .iter()
                .filter(|a| a.name.ends_with(".exe") && a.name.contains("win"))
                .collect();
            if win_exe.is_empty() {
                return None;
            }
            let is_portable = |a: &GithubAsset| a.name.to_lowercase().contains("portable");
            let is_setup = |a: &GithubAsset| a.name.to_lowercase().contains("setup");
            // 安装(NSIS)：setup → 非 portable → 首个。
            win_exe
                .iter()
                .copied()
                .find(|a| is_setup(a))
                .or_else(|| win_exe.iter().copied().find(|a| !is_portable(a)))
                .or_else(|| win_exe.first().copied())
        }
        AssetPlatform::Macos => {
            let arch_pattern = if arch == AssetArch::Arm64 {
                "mac-arm64"
            } else {
                "mac-x64"
            };
            // **无回落**（2026-07-21 用户裁定）：分架构单出后，「任意 .dmg」回落会在 release 缺
            // 一份（某个 mac job 挂掉）时把另一架构的包发给用户 —— arm64 包在 Intel 上根本执行
            // 不了，x64 包在 Apple Silicon 上走 Rosetta 且内核错配。宁可不更新，也不发错架构。
            // 返回 None ⇒ check_app_update 走 AppUpdateCheck::NoUpdate（= 上游「未找到适合当前
            // 平台的安装包」），不是报错。
            assets
                .iter()
                .find(|a| a.name.contains(arch_pattern) && a.name.ends_with(".dmg"))
        }
        AssetPlatform::Linux => {
            let app_image: Vec<&GithubAsset> = assets
                .iter()
                .filter(|a| a.name.ends_with(".AppImage"))
                .collect();
            let deb: Vec<&GithubAsset> =
                assets.iter().filter(|a| a.name.ends_with(".deb")).collect();
            if loose_form {
                // AppImage(loose)：AppImage → .deb。
                app_image.first().copied().or_else(|| deb.first().copied())
            } else {
                // deb 安装：.deb → AppImage。
                deb.first().copied().or_else(|| app_image.first().copied())
            }
        }
    }
}

// ── 内核资产选择（移植 singbox-asset.findSuitableSingboxAsset，逐字保留）──────────────

/// 从 release 资产里挑适配 `(platform, arch)` 的 **sing-box 内核**构建。
///
/// 移植自 `singbox-asset.findSuitableSingboxAsset`：
///  1. 平台关键词（windows/darwin/linux）+ 架构关键词（amd64/arm64）+ 后缀（平台默认 ext 或 `.zip`）过滤；
///  2. 命中集合内按优先级取：① 含 `with-naive`/`full`（带 naive 出站）② 非 `legacy` ③ 首个命中。
///
/// 架构为 [`Other`](AssetArch::Other) 时架构关键词为空串（`contains("")` 恒真，= 上游 `archKeyword=''`
/// 时 `.includes('')` 恒真），即不按架构过滤。无任何命中返回 `None`。
#[must_use]
pub fn find_suitable_singbox_asset(
    assets: &[GithubAsset],
    platform: AssetPlatform,
    arch: AssetArch,
) -> Option<&GithubAsset> {
    let (keyword, ext) = match platform {
        AssetPlatform::Windows => ("windows", ".zip"),
        AssetPlatform::Macos => ("darwin", ".tar.gz"),
        AssetPlatform::Linux => ("linux", ".tar.gz"),
    };
    let arch_keyword = match arch {
        AssetArch::X64 => "amd64",
        AssetArch::Arm64 => "arm64",
        AssetArch::Other => "",
    };

    let filtered: Vec<&GithubAsset> = assets
        .iter()
        .filter(|a| {
            let lower = a.name.to_lowercase();
            lower.contains(keyword)
                && lower.contains(arch_keyword)
                && (a.name.ends_with(ext) || a.name.ends_with(".zip"))
        })
        .collect();
    if filtered.is_empty() {
        return None;
    }

    // 1. with-naive / full 优先。
    if let Some(a) = filtered.iter().copied().find(|a| {
        let lower = a.name.to_lowercase();
        lower.contains("with-naive") || lower.contains("full")
    }) {
        return Some(a);
    }
    // 2. 非 legacy。
    if let Some(a) = filtered
        .iter()
        .copied()
        .find(|a| !a.name.to_lowercase().contains("legacy"))
    {
        return Some(a);
    }
    // 3. 首个命中。
    filtered.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, size: u64) -> GithubAsset {
        GithubAsset {
            name: name.to_string(),
            browser_download_url: format!("https://github.com/o/r/releases/download/x/{name}"),
            size,
            digest: None,
        }
    }

    #[test]
    fn parse_asset_digest_accepts_only_wellformed_sha256() {
        let hex = "a".repeat(64);
        assert_eq!(
            parse_asset_digest(&format!("sha256:{hex}")),
            Some(hex.clone())
        );
        // 大写规范化为小写（便于簿记比对；verify_bytes 本身大小写不敏感）。
        assert_eq!(
            parse_asset_digest(&format!("sha256:{}", "AB".repeat(32))),
            Some("ab".repeat(32))
        );
        // **逃逸用例**：长度不对 / 非 hex / 换算法 / 无前缀 / 空 → 一律 None。
        // 若被当成 sha256 传给 verify_bytes，会把「拿不到可用摘要」伪装成「校验失败」。
        assert_eq!(parse_asset_digest("sha256:abcd"), None);
        assert_eq!(
            parse_asset_digest(&format!("sha256:{}", "z".repeat(64))),
            None
        );
        assert_eq!(parse_asset_digest(&format!("blake3:{hex}")), None);
        assert_eq!(parse_asset_digest(&hex), None);
        assert_eq!(parse_asset_digest(""), None);
    }

    #[test]
    fn check_app_update_threads_digest_into_sha256_but_tolerates_absence() {
        let hex = "b".repeat(64);
        let json = format!(
            r#"[{{"tag_name":"v2.0.0","published_at":"2024-05-01T00:00:00Z","assets":[
                {{"name":"polaris_2.0.0_amd64.deb","browser_download_url":"https://x/d","size":9,
                  "digest":"sha256:{hex}"}}]}}]"#
        );
        let r = check_app_update(
            &json,
            "1.0.0",
            false,
            None,
            AssetPlatform::Linux,
            AssetArch::X64,
            false,
        )
        .unwrap();
        let AppUpdateCheck::Available(info) = r else {
            panic!("应有更新");
        };
        assert_eq!(info.sha256.as_deref(), Some(hex.as_str()));

        // 旧 release 无 digest → sha256=None，但**仍然报有更新**（缺摘要不阻断更新）。
        let json = r#"[{"tag_name":"v2.0.0","published_at":"2024-05-01T00:00:00Z","assets":[
            {"name":"polaris_2.0.0_amd64.deb","browser_download_url":"https://x/d","size":9}]}]"#;
        let r = check_app_update(
            json,
            "1.0.0",
            false,
            None,
            AssetPlatform::Linux,
            AssetArch::X64,
            false,
        )
        .unwrap();
        let AppUpdateCheck::Available(info) = r else {
            panic!("缺 digest 不得让更新消失");
        };
        assert_eq!(info.sha256, None);
    }

    #[test]
    fn asset_digest_is_optional_and_parses_when_present() {
        // 旧 release 无 digest 字段 → 必须解析成功（缺摘要不是错误，回落 Content-Length 校验）。
        let no_digest = r#"[{"tag_name":"v2.0.0","published_at":"2024-05-01T00:00:00Z",
            "assets":[{"name":"polaris_2.0.0_amd64.deb","browser_download_url":"https://x/d","size":1}]}]"#;
        let rs: Vec<GithubRelease> = serde_json::from_str(no_digest).unwrap();
        assert_eq!(rs[0].assets[0].digest, None);

        let with_digest = r#"[{"tag_name":"v2.0.0","published_at":"2024-05-01T00:00:00Z",
            "assets":[{"name":"a.deb","browser_download_url":"https://x/d","size":1,
            "digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000"}]}]"#;
        let rs: Vec<GithubRelease> = serde_json::from_str(with_digest).unwrap();
        assert!(rs[0].assets[0]
            .digest
            .as_deref()
            .unwrap()
            .starts_with("sha256:"));
    }

    // ── 更新源常量 / URL ──────────────────────────────────────────────────────

    #[test]
    fn source_repo_constants_are_the_polaris_and_sagernet_repos() {
        // 更新源仓库是全链路的锚：改错 = 检查更新指向错误的 repo（拉不到或拉到别人的 release）。
        assert_eq!(APP_UPDATE_REPO, ("Sway-Chan", "polaris"));
        assert_eq!(CORE_UPDATE_REPO, ("SagerNet", "sing-box"));
        assert_eq!(
            github_releases_api_url("Sway-Chan", "polaris"),
            "https://api.github.com/repos/Sway-Chan/polaris/releases"
        );
        // 便携资产前缀是三处（package.yml 产出 / 本模块选包 / verify-packaging.mjs 断言）
        // 共用的命名契约，改它必须三处同改，故在此钉死字面值。
        assert_eq!(PORTABLE_ZIP_PREFIX, "polaris-portable-");
    }

    #[test]
    fn platform_and_arch_mapping_from_std_env_consts() {
        assert_eq!(
            AssetPlatform::from_os("windows"),
            Some(AssetPlatform::Windows)
        );
        assert_eq!(AssetPlatform::from_os("macos"), Some(AssetPlatform::Macos));
        assert_eq!(AssetPlatform::from_os("linux"), Some(AssetPlatform::Linux));
        assert_eq!(AssetPlatform::from_os("freebsd"), None);
        assert_eq!(AssetArch::from_arch("x86_64"), AssetArch::X64);
        assert_eq!(AssetArch::from_arch("aarch64"), AssetArch::Arm64);
        assert_eq!(AssetArch::from_arch("riscv64"), AssetArch::Other);
    }

    // ── App 安装包资产选择真值表（findSuitableUpdateAsset）────────────────────

    /// 一个 release 的**真实资产集**（6 个，= README「各恰好一个」那张表 + `package.yml` 的实际产物名）。
    ///
    /// 🔴 用真产物名，不用理想化名字：本函数此前的 Windows 测试用的是
    /// `Polaris-0.2.0-win-x64-portable.exe` —— 一个**打包链从未产出过**的名字（便携产物是 zip）。
    /// 测试在虚构输入上绿，真资产集下 loose 形态却恒命中 NSIS setup，缺陷因此活过了「全绿」。
    fn release_assets() -> Vec<GithubAsset> {
        vec![
            asset("Polaris_0.2.0_x64-win-setup.exe", 100),
            asset("polaris-portable-v0.2.0.zip", 90),
            asset("Polaris_0.2.0_aarch64-mac-arm64.dmg", 110),
            asset("Polaris_0.2.0_x64-mac-x64.dmg", 111),
            asset("polaris_0.2.0_amd64.deb", 80),
            asset("Polaris_0.2.0_amd64.AppImage", 81),
        ]
    }

    /// 🔴 回归门（2026-07-22 修 #72 形态错配本体）：便携形态必须拿到**便携产物**。
    ///
    /// 变异探针（每条覆盖一条独立逃逸路径，单条不足）：
    ///  - loose 分支改回共用 `.exe && contains("win")` 候选集 ⇒ ①③ 转红；
    ///  - loose 分支尾部补任何一级 `.or_else(...)` 回落到 `.exe` ⇒ ② 转红；
    ///  - 两个分支对调 ⇒ ①④ 转红；
    ///  - 前缀判据写成大小写不敏感 / `contains(".zip")` 而非 `ends_with` ⇒ ⑤⑥ 转红。
    #[test]
    fn update_asset_windows_loose_picks_portable_zip_never_an_installer() {
        let assets = release_assets();

        // ① 便携形态：拿到 zip，**不是** setup。
        let loose =
            find_suitable_update_asset(&assets, AssetPlatform::Windows, AssetArch::X64, true)
                .expect("便携形态必须能选到 polaris-portable-*.zip");
        assert_eq!(
            loose.name, "polaris-portable-v0.2.0.zip",
            "便携用户拿到安装器 = #72 形态错配本体（装出与便携副本并存的第二份程序）"
        );

        // ② 便携产物缺失（portable 那步挂了）：**必须 None**，绝不回落任一 `.exe`。
        //    宁可不更新，也不发错形态包 —— 与 macOS「不发错架构包」同一条纪律。
        let no_zip: Vec<GithubAsset> = release_assets()
            .into_iter()
            .filter(|a| !a.name.ends_with(".zip"))
            .collect();
        assert!(
            find_suitable_update_asset(&no_zip, AssetPlatform::Windows, AssetArch::X64, true)
                .is_none(),
            "便携形态选不到 zip 时必须返回 None，不得回落 NSIS setup"
        );

        // ③ 安装形态：仍然是 downloadBootstrapper 安装器，**不是** zip（命名契约一字未动）。
        let installed =
            find_suitable_update_asset(&assets, AssetPlatform::Windows, AssetArch::X64, false)
                .expect("安装形态必须能选到 bootstrapper");
        assert_eq!(installed.name, "Polaris_0.2.0_x64-win-setup.exe");

        // ④ 安装形态下便携 zip 存在也绝不被选中（两条规则不相交）。
        assert!(!installed.name.ends_with(".zip"));

        // ⑤ 前缀大小写敏感：非 `package.yml` 那个字面名的 zip 不得被当成便携产物。
        let wrong_case = vec![asset("Polaris-Portable-v0.2.0.zip", 90)];
        assert!(
            find_suitable_update_asset(&wrong_case, AssetPlatform::Windows, AssetArch::X64, true)
                .is_none(),
            "判据须与 package.yml 的字面产物名同口径（大小写敏感）"
        );

        // ⑥ `--clobber` 失效产生的 `.zip.1` 重复资产不得被选中（判据是 `ends_with`，不是 `contains`）。
        let clobber_dupe = vec![asset("polaris-portable-v0.2.0.zip.1", 90)];
        assert!(
            find_suitable_update_asset(&clobber_dupe, AssetPlatform::Windows, AssetArch::X64, true)
                .is_none(),
            "`.zip.1` 不是可用产物，不得被便携规则命中"
        );
    }

    #[test]
    fn update_asset_windows_none_when_no_win_exe() {
        let assets = vec![
            asset("Polaris-0.2.0.dmg", 100),
            asset("Polaris-0.2.0.AppImage", 100),
        ];
        assert!(
            find_suitable_update_asset(&assets, AssetPlatform::Windows, AssetArch::X64, false)
                .is_none()
        );
        // 便携形态同样无适配（没有 zip）。
        assert!(
            find_suitable_update_asset(&assets, AssetPlatform::Windows, AssetArch::X64, true)
                .is_none()
        );
    }

    /// 非 Windows 平台的选包**不受本次改动影响**：同一份真实资产集下 mac 双架构各自命中、
    /// Linux 双形态各自命中。（防「顺手把 zip 规则套到别的平台」这类逃逸。）
    #[test]
    fn update_asset_other_platforms_unaffected_by_windows_portable_rule() {
        let assets = release_assets();
        let arm = find_suitable_update_asset(&assets, AssetPlatform::Macos, AssetArch::Arm64, true)
            .unwrap();
        assert_eq!(arm.name, "Polaris_0.2.0_aarch64-mac-arm64.dmg");
        let x64 = find_suitable_update_asset(&assets, AssetPlatform::Macos, AssetArch::X64, false)
            .unwrap();
        assert_eq!(x64.name, "Polaris_0.2.0_x64-mac-x64.dmg");
        let loose_linux =
            find_suitable_update_asset(&assets, AssetPlatform::Linux, AssetArch::X64, true)
                .unwrap();
        assert_eq!(loose_linux.name, "Polaris_0.2.0_amd64.AppImage");
        let inst_linux =
            find_suitable_update_asset(&assets, AssetPlatform::Linux, AssetArch::X64, false)
                .unwrap();
        assert_eq!(inst_linux.name, "polaris_0.2.0_amd64.deb");
    }

    /// 正常路径：双 dmg 齐全时两个架构各自命中，互不交叉。
    ///
    /// 名字用 CI 真实产出（`package.yml` 的 `Tag macOS dmg with arch` 步：Tauri 默认名
    /// `Polaris_<ver>_<triple-arch>.dmg` 追加 `-<mac_arch_tag>`），避免测试用理想化名字
    /// 通过、真产物名不通过。注意 arm64 那份名里含 `aarch64`、x64 那份含 `x64`。
    #[test]
    fn update_asset_macos_picks_own_arch_dmg_when_both_present() {
        let assets = vec![
            asset("Polaris_0.2.0_aarch64-mac-arm64.dmg", 100),
            asset("Polaris_0.2.0_x64-mac-x64.dmg", 100),
        ];
        let arm =
            find_suitable_update_asset(&assets, AssetPlatform::Macos, AssetArch::Arm64, false)
                .unwrap();
        assert_eq!(arm.name, "Polaris_0.2.0_aarch64-mac-arm64.dmg");
        let x64 = find_suitable_update_asset(&assets, AssetPlatform::Macos, AssetArch::X64, false)
            .unwrap();
        assert_eq!(x64.name, "Polaris_0.2.0_x64-mac-x64.dmg");
        // 形态（loose_form）不参与 macOS 选包：两个形态选到同一份。
        assert_eq!(
            find_suitable_update_asset(&assets, AssetPlatform::Macos, AssetArch::Arm64, true)
                .unwrap()
                .name,
            arm.name
        );
    }

    /// 🔴 回归门（2026-07-21 用户裁定「宁可不更新，也不发错架构包」）：
    /// release 里某个架构的 dmg 缺失（该 mac job 挂掉）时，**必须返回 None**，
    /// 不得回落到另一架构那份 —— 否则 x64 用户会静默拿到 arm64 包。
    ///
    /// 这两条是把 `.or_else(|| assets.iter().find(|a| a.name.ends_with(".dmg")))`
    /// 加回去就会转红的变异探针（两个方向各一条，单向探针会漏掉对称的另一半）。
    #[test]
    fn update_asset_macos_returns_none_rather_than_cross_arch_dmg() {
        // 只剩 arm64 → x64 请求返回 None（不得拿到 arm64 包）。
        let only_arm = vec![asset("Polaris_0.2.0_aarch64-mac-arm64.dmg", 100)];
        assert!(
            find_suitable_update_asset(&only_arm, AssetPlatform::Macos, AssetArch::X64, false)
                .is_none(),
            "x64 请求在只有 arm64 dmg 时必须返回 None，不得回落跨架构包"
        );
        // 只剩 x64 → arm64 请求返回 None（对称方向）。
        let only_x64 = vec![asset("Polaris_0.2.0_x64-mac-x64.dmg", 100)];
        assert!(
            find_suitable_update_asset(&only_x64, AssetPlatform::Macos, AssetArch::Arm64, false)
                .is_none(),
            "arm64 请求在只有 x64 dmg 时必须返回 None，不得回落跨架构包"
        );
        // 无架构标记的裸 dmg（改名步失效的产物）同样不得被任一架构选中。
        let untagged = vec![asset("Polaris_0.2.0_aarch64.dmg", 100)];
        assert!(find_suitable_update_asset(
            &untagged,
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false
        )
        .is_none());
        assert!(
            find_suitable_update_asset(&untagged, AssetPlatform::Macos, AssetArch::X64, false)
                .is_none()
        );
    }

    #[test]
    fn update_asset_linux_loose_picks_appimage_installed_picks_deb() {
        let assets = vec![
            asset("polaris_0.2.0_amd64.deb", 100),
            asset("Polaris-0.2.0.AppImage", 90),
        ];
        assert!(
            find_suitable_update_asset(&assets, AssetPlatform::Linux, AssetArch::X64, true)
                .unwrap()
                .name
                .ends_with(".AppImage")
        );
        assert!(
            find_suitable_update_asset(&assets, AssetPlatform::Linux, AssetArch::X64, false)
                .unwrap()
                .name
                .ends_with(".deb")
        );
        // installed 但只有 AppImage → 回落 AppImage。
        let only_img = vec![asset("Polaris-0.2.0.AppImage", 90)];
        assert!(
            find_suitable_update_asset(&only_img, AssetPlatform::Linux, AssetArch::X64, false)
                .unwrap()
                .name
                .ends_with(".AppImage")
        );
    }

    // ── 内核资产选择真值表（findSuitableSingboxAsset）─────────────────────────

    #[test]
    fn singbox_asset_matches_platform_arch_and_prefers_naive() {
        let assets = vec![
            asset("sing-box-1.14.0-linux-amd64.tar.gz", 10),
            asset("sing-box-1.14.0-linux-amd64-with-naive.tar.gz", 12),
            asset("sing-box-1.14.0-linux-amd64-legacy.tar.gz", 9),
            asset("sing-box-1.14.0-darwin-arm64.tar.gz", 10),
        ];
        // linux/amd64 + with-naive 优先。
        let picked =
            find_suitable_singbox_asset(&assets, AssetPlatform::Linux, AssetArch::X64).unwrap();
        assert!(picked.name.contains("with-naive"));
    }

    #[test]
    fn singbox_asset_non_legacy_then_first_when_no_naive() {
        let assets = vec![
            asset("sing-box-1.14.0-windows-amd64-legacy.zip", 9),
            asset("sing-box-1.14.0-windows-amd64.zip", 10),
        ];
        // 无 naive → 非 legacy 优先。
        let picked =
            find_suitable_singbox_asset(&assets, AssetPlatform::Windows, AssetArch::X64).unwrap();
        assert!(!picked.name.contains("legacy"));
    }

    #[test]
    fn singbox_asset_none_when_no_platform_match() {
        let assets = vec![asset("sing-box-1.14.0-darwin-arm64.tar.gz", 10)];
        // 找 windows/amd64 → 无命中。
        assert!(
            find_suitable_singbox_asset(&assets, AssetPlatform::Windows, AssetArch::X64).is_none()
        );
    }

    // ── App 检查全链路（check_app_update）─────────────────────────────────────

    /// 两个 release 的样本 JSON（GitHub 原始形状）：0.1.0（旧）+ 0.2.0（新，含多平台资产）。
    fn sample_releases_json() -> String {
        r#"[
          {
            "tag_name": "v0.2.0",
            "name": "Polaris 0.2.0",
            "body": "新版说明",
            "prerelease": false,
            "published_at": "2024-05-01T12:00:00Z",
            "assets": [
              {"name": "Polaris-0.2.0-mac-arm64.dmg", "browser_download_url": "https://x/mac", "size": 12345},
              {"name": "Polaris-Setup-0.2.0-win-x64.exe", "browser_download_url": "https://x/win", "size": 999},
              {"name": "polaris_0.2.0_amd64.deb", "browser_download_url": "https://x/deb", "size": 555}
            ]
          },
          {
            "tag_name": "v0.1.0",
            "name": "Polaris 0.1.0",
            "prerelease": false,
            "published_at": "2024-01-01T00:00:00Z",
            "assets": []
          }
        ]"#
        .to_string()
    }

    #[test]
    fn check_app_update_returns_available_with_faithful_fields() {
        let json = sample_releases_json();
        let r = check_app_update(
            &json,
            "0.1.0",
            false,
            None,
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .unwrap();
        match r {
            AppUpdateCheck::Available(info) => {
                assert_eq!(
                    info.version, "v0.2.0",
                    "version 保留原始 tag（含 v，对齐 上游）"
                );
                assert_eq!(info.title, "Polaris 0.2.0");
                assert_eq!(info.release_notes, "新版说明");
                assert_eq!(info.download_url, "https://x/mac");
                assert_eq!(info.file_size, 12345);
                assert_eq!(info.published_at, "2024-05-01T12:00:00Z");
                assert!(!info.is_prerelease);
                assert_eq!(info.file_name, "Polaris-0.2.0-mac-arm64.dmg");
            }
            AppUpdateCheck::NoUpdate => panic!("应发现 0.2.0 更新"),
        }
    }

    #[test]
    fn check_app_update_no_update_when_current_is_latest() {
        let json = sample_releases_json();
        // 当前已是 0.2.0 → 无更新。
        let r = check_app_update(
            &json,
            "0.2.0",
            false,
            None,
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .unwrap();
        assert_eq!(r, AppUpdateCheck::NoUpdate);
    }

    #[test]
    fn check_app_update_skipped_version_is_no_update() {
        let json = sample_releases_json();
        // 跳过 0.2.0（存的是去 v 的版本）→ 无更新。
        let r = check_app_update(
            &json,
            "0.1.0",
            false,
            Some("0.2.0"),
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .unwrap();
        assert_eq!(r, AppUpdateCheck::NoUpdate);

        // W8 反例（同口径门的另一半）：存**原始 tag**（`v0.2.0`，修复前两个写点的实存形态）
        // ⇒ 与比较侧 strip_v 后的值永不相等 ⇒ 照常报 Available。若有人把比较侧改回原始 tag
        // 来「修」跳过，这半句转红——口径必须由写侧归一化（stored_skip_version），
        // 不是把比侧改脏。
        let raw = check_app_update(
            &json,
            "0.1.0",
            false,
            Some("v0.2.0"),
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .unwrap();
        assert!(
            matches!(raw, AppUpdateCheck::Available(_)),
            "原始 tag 不该命中跳过——命中了说明比较侧被改回原始 tag 口径"
        );
    }

    #[test]
    fn check_app_update_prerelease_filtered_unless_included() {
        let json = r#"[
          {"tag_name":"v0.3.0-beta.1","prerelease":true,"published_at":"2024-06-01T00:00:00Z",
           "assets":[{"name":"Polaris-0.3.0-mac-arm64.dmg","browser_download_url":"https://x/beta","size":1}]},
          {"tag_name":"v0.2.0","prerelease":false,"published_at":"2024-05-01T00:00:00Z",
           "assets":[{"name":"Polaris-0.2.0-mac-arm64.dmg","browser_download_url":"https://x/stable","size":1}]}
        ]"#;
        // include_prerelease=false → beta 被过滤，最新正式 = 0.2.0。
        let stable = check_app_update(
            json,
            "0.1.0",
            false,
            None,
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .unwrap();
        assert!(matches!(stable, AppUpdateCheck::Available(ref i) if i.version == "v0.2.0"));
        // include_prerelease=true → 取最新发布 = beta。
        let beta = check_app_update(
            json,
            "0.1.0",
            true,
            None,
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .unwrap();
        assert!(matches!(beta, AppUpdateCheck::Available(ref i) if i.version == "v0.3.0-beta.1"));
    }

    #[test]
    fn check_app_update_no_suitable_asset_is_no_update() {
        // 有更新但当前平台无适配资产（release 只有 mac dmg，平台是 windows）→ 无更新（非报错）。
        let json = sample_releases_json();
        let r = check_app_update(
            &json,
            "0.1.0",
            false,
            None,
            AssetPlatform::Windows,
            AssetArch::X64,
            false,
        )
        .unwrap();
        // 注：sample 里有 win-x64.exe → windows 应能匹配到 setup 包，故这里改用只有 dmg 的 release 断言无资产。
        // （sample 覆盖三平台，windows 有 setup → 会命中。故此断言其实是 Available；用独立样本测无资产。）
        assert!(matches!(r, AppUpdateCheck::Available(_)));

        let mac_only = r#"[{"tag_name":"v0.2.0","prerelease":false,"published_at":"2024-05-01T00:00:00Z",
          "assets":[{"name":"Polaris-0.2.0-mac-arm64.dmg","browser_download_url":"https://x/mac","size":1}]}]"#;
        let none = check_app_update(
            mac_only,
            "0.1.0",
            false,
            None,
            AssetPlatform::Windows,
            AssetArch::X64,
            false,
        )
        .unwrap();
        assert_eq!(none, AppUpdateCheck::NoUpdate);
    }

    /// 全链路：Windows 便携形态请求 → `updateInfo` 指向便携 zip（下载 URL / 文件名都是它）。
    ///
    /// 只测 `find_suitable_update_asset` 不够：`check_app_update` 是宿主真正调的入口，
    /// 形态参数得**一路传到底**才有意义（少传一层 = 选包器修好了、用户仍拿安装器）。
    #[test]
    fn check_app_update_windows_loose_form_yields_portable_zip() {
        let json = r#"[{"tag_name":"v0.2.0","prerelease":false,"published_at":"2024-05-01T00:00:00Z",
          "assets":[
            {"name":"Polaris_0.2.0_x64-win-setup.exe","browser_download_url":"https://x/win","size":1},
            {"name":"polaris-portable-v0.2.0.zip","browser_download_url":"https://x/zip","size":3}]}]"#;

        let loose = check_app_update(
            json,
            "0.1.0",
            false,
            None,
            AssetPlatform::Windows,
            AssetArch::X64,
            true,
        )
        .unwrap();
        let AppUpdateCheck::Available(info) = loose else {
            panic!("便携形态应发现更新（便携 zip 在 release 里）");
        };
        assert_eq!(info.file_name, "polaris-portable-v0.2.0.zip");
        assert_eq!(info.download_url, "https://x/zip");

        // 安装形态在同一份 release 上仍拿 bootstrapper。
        let installed = check_app_update(
            json,
            "0.1.0",
            false,
            None,
            AssetPlatform::Windows,
            AssetArch::X64,
            false,
        )
        .unwrap();
        let AppUpdateCheck::Available(info) = installed else {
            panic!("安装形态应发现更新");
        };
        assert_eq!(info.file_name, "Polaris_0.2.0_x64-win-setup.exe");

        // 便携产物缺失 ⇒ 便携用户如实「无更新」，**不是**被发安装器。
        let no_zip = r#"[{"tag_name":"v0.2.0","prerelease":false,"published_at":"2024-05-01T00:00:00Z",
          "assets":[{"name":"Polaris_0.2.0_x64-win-setup.exe","browser_download_url":"https://x/win","size":1}]}]"#;
        let r = check_app_update(
            no_zip,
            "0.1.0",
            false,
            None,
            AssetPlatform::Windows,
            AssetArch::X64,
            true,
        )
        .unwrap();
        assert_eq!(r, AppUpdateCheck::NoUpdate);
    }

    #[test]
    fn check_app_update_malformed_json_errors() {
        let err = check_app_update(
            "{not json",
            "0.1.0",
            false,
            None,
            AssetPlatform::Linux,
            AssetArch::X64,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::ParseJson(_)));
    }

    #[test]
    fn check_app_update_empty_releases_is_no_update() {
        let r = check_app_update(
            "[]",
            "0.1.0",
            false,
            None,
            AssetPlatform::Linux,
            AssetArch::X64,
            false,
        )
        .unwrap();
        assert_eq!(r, AppUpdateCheck::NoUpdate);
    }
}
