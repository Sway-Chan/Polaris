//! 版本清单解析（最新版本号 / 下载 URL / SHA256）。
//!
//! 移植自两处 Polaris release 解析：
//!   - `CoreUpdateService.checkUpdate`（`CoreUpdateService.ts:152-267`）：拉 GitHub releases → 过滤 prerelease
//!     → 取最新 → 去掉 `v` 前缀 → compareVersions 判新 → findSuitableAsset 挑平台资产。
//!   - `UpdateService.checkForUpdate`（`UpdateService.ts:151-245`）：同构的 app 自更新检查。
//!
//! Polaris 用 GitHub releases API（`api.github.com/repos/.../releases`），无独立「清单」文件。本 crate 把
//! release 解析抽成 [`VersionManifest`]：支持两种形态——
//!   1. GitHub release JSON（生产侧：`fetchReleases` 拉回的数组，本 crate 只做纯解析）。
//!   2. 自定义 sha256 清单（JSON：`{ version, url, sha256, optional notes/prerelease }`）——
//!      用于 staged 更新周期里「下载前已知期望 hash」的校验流（Polaris 原本下载后才知道 size、靠
//!      content-length 防截断；本 crate 增强为 sha256 强校验，manifest 提前携带 hash）。
//!
//! 资产挑选（平台/架构匹配）移植自 `singbox-asset.findSuitableSingboxAsset` /
//! `update-asset.findSuitableUpdateAsset` 的纯逻辑，但**本 crate 只暴露 selector trait**
//! （[`AssetSelector`]）：具体平台规则在宿主 crate 注入（避免在本纯逻辑 crate 硬编码 process.platform/arch，
//! 保持可单测）。

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::version::{compare_semver, is_newer, same_major_minor};

/// 清单解析错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ManifestError {
    /// JSON 解析失败（= 上游 `JSON.parse(data)` catch → `解析 GitHub 响应失败`）。
    #[error("manifest JSON parse error: {0}")]
    ParseJson(String),
    /// release 列表为空或无正式版（= 上游 `未找到发布版本` / `未找到正式版本`）。
    #[error("no valid releases in manifest")]
    EmptyReleases,
    /// 无适配当前平台的资产（= 上游 `未找到适合当前平台的构建`）。
    #[error("no suitable asset for target platform")]
    NoSuitableAsset,
    /// 版本号非法（无法解析）。
    #[error("invalid version string: {0}")]
    InvalidVersion(String),
    /// sha256 字段非法（非 64 字符 hex）。
    #[error("invalid sha256 in manifest entry: {0}")]
    InvalidSha256(String),
}

/// 清单条目：一个版本的完整下载描述（含 sha256 强校验字段）。
///
/// Polaris 的 `UpdateInfo` / `CoreUpdateCheckResult` 不含 sha256（靠 content-length 防截断）。本 crate
/// 增强为：当 manifest 携带 sha256 时，staged 周期在下载后做强校验（防中间人篡改，比 size 校验更强）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifestEntry {
    /// 版本号（去 `v` 前缀，对齐 上游 `tag_name.replace(/^v/, '')`）。
    pub version: String,
    /// 下载 URL（GitHub `browser_download_url` 或自建镜像）。
    pub url: String,
    /// 期望的 sha256（hex，64 字符）。staged 周期下载后强校验；可选（None = 仅靠 size 校验，回落 Polaris 行为）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// 是否预览版（= 上游 `prerelease`；正式版过滤后才进 manifest）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
    /// 发布说明（= 上游 `body` / `releaseNotes`）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

impl VersionManifestEntry {
    /// 校验 entry 的内联完整性（version 非空 + sha256 若存在须合法）。
    ///
    /// # Errors
    ///
    /// - [`ManifestError::InvalidVersion`]：version 空或无法解析。
    /// - [`ManifestError::InvalidSha256`]：sha256 字段存在但非 64 字符 hex。
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.version.trim().is_empty() {
            return Err(ManifestError::InvalidVersion(self.version.clone()));
        }
        if compare_semver(&self.version, &self.version).is_err() {
            return Err(ManifestError::InvalidVersion(self.version.clone()));
        }
        if let Some(hash) = &self.sha256 {
            if !crate::verify::is_valid_sha256_hex(hash) {
                return Err(ManifestError::InvalidSha256(hash.clone()));
            }
        }
        Ok(())
    }
}

impl fmt::Display for VersionManifestEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{} ({})", self.version, self.url)?;
        if self.prerelease {
            f.write_str(" [prerelease]")?;
        }
        Ok(())
    }
}

/// 版本清单：一组 release 条目 + 当前版本基准，提供「找最新」的纯逻辑。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifest {
    /// 全部条目（含预览版，过滤由 [`VersionManifest::latest`] 按参数决定）。
    pub entries: Vec<VersionManifestEntry>,
}

impl VersionManifest {
    /// 从自定义 sha256 清单 JSON 解析（单版本形态：`{ version, url, sha256, ... }`）。
    ///
    /// # Errors
    ///
    /// - [`ManifestError::ParseJson`]：JSON 解析失败。
    /// - [`ManifestError::InvalidVersion`] / [`ManifestError::InvalidSha256`]：entry 内联完整性校验失败。
    pub fn from_single_json(json: &str) -> Result<Self, ManifestError> {
        let entry: VersionManifestEntry =
            serde_json::from_str(json).map_err(|e| ManifestError::ParseJson(e.to_string()))?;
        entry.validate()?;
        Ok(Self {
            entries: vec![entry],
        })
    }

    /// 从条目数组 JSON 解析（`[{version,url,sha256,...}, ...]`）。
    ///
    /// # Errors
    ///
    /// 透传 [`VersionManifestEntry::validate`] + [`ManifestError::ParseJson`]。
    pub fn from_array_json(json: &str) -> Result<Self, ManifestError> {
        let entries: Vec<VersionManifestEntry> =
            serde_json::from_str(json).map_err(|e| ManifestError::ParseJson(e.to_string()))?;
        for e in &entries {
            e.validate()?;
        }
        Ok(Self { entries })
    }

    /// 从已有条目列表构造（便利方法，测试用）。
    #[must_use]
    pub fn from_entries(entries: Vec<VersionManifestEntry>) -> Self {
        Self { entries }
    }

    /// 找最新版本条目（移植自 上游 `validReleases[0]` + `compareVersions` 判新逻辑）。
    ///
    /// 语义（逐字对齐 `CoreUpdateService.checkUpdate:181-196`）：
    ///   - `include_prerelease = false` → 过滤掉 prerelease（上游 `validReleases.filter(!prerelease)`）。
    ///   - 在剩余条目里找比 `current` 新的、版本号最大的那个。
    ///   - `cross_band = Some(true)` 时仅返回与 `current` 同 major.minor 的最新（兼容带内，= Polaris
    ///     `sameMajorMinor` 硬闸）；`None` 或 `Some(false)` 不限带。
    ///
    /// # Errors
    ///
    /// - [`ManifestError::InvalidVersion`]：`current` 为空串。
    pub fn latest(
        &self,
        current: &str,
        include_prerelease: bool,
        cross_band: Option<bool>,
    ) -> Result<Option<&VersionManifestEntry>, ManifestError> {
        // current 合法性：空串显式报错（Polaris compareVersions 对非空串容忍，但空串无意义）。
        // 注：current = "未知" 时 parse_semver 返回 main=空→按 [0] 处理，compare_semver("未知","0.0.0")=0，
        // 即「未知」等价于「0.0.0」——任何正式版都算新。此处不拦截（与 Polaris 容忍口径一致）。
        if current.trim().is_empty() {
            return Err(ManifestError::InvalidVersion(current.to_string()));
        }

        let restrict_band = cross_band.unwrap_or(false);

        let mut best: Option<&VersionManifestEntry> = None;
        for e in &self.entries {
            // 过滤预览版（include_prerelease=false 时跳过 prerelease 条目）。
            if !include_prerelease && e.prerelease {
                continue;
            }
            // 兼容带硬闸：restrict 时跳过跨带版本（= Polaris 自动路径的 sameMajorMinor 闸）。
            if restrict_band && !same_major_minor(&e.version, current).unwrap_or(false) {
                continue;
            }
            // 必须比 current 新。
            if !is_newer(&e.version, current).unwrap_or(false) {
                continue;
            }
            // 比 best 更新才更新 best。
            match best {
                None => best = Some(e),
                Some(b) => {
                    if is_newer(&e.version, &b.version).unwrap_or(false) {
                        best = Some(e);
                    }
                }
            }
        }
        Ok(best)
    }

    /// 找最新且必须比 current 新的条目，返回 owned clone（便利方法）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::latest`]。
    pub fn latest_newer(
        &self,
        current: &str,
        include_prerelease: bool,
        cross_band: Option<bool>,
    ) -> Result<Option<VersionManifestEntry>, ManifestError> {
        Ok(self
            .latest(current, include_prerelease, cross_band)?
            .cloned())
    }

    /// 是否存在比 current 新的版本（带内或全部，按 cross_band）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::latest`]。
    pub fn has_update(
        &self,
        current: &str,
        include_prerelease: bool,
        cross_band: Option<bool>,
    ) -> Result<bool, ManifestError> {
        Ok(self
            .latest(current, include_prerelease, cross_band)?
            .is_some())
    }
}

/// 资产选择 trait：从一组资产名里挑出适配「目标平台/架构」的。
///
/// 移植自 上游 `findSuitableSingboxAsset` / `findSuitableUpdateAsset`（`singbox-asset.ts` /
/// `update-asset.ts`）。**本 crate 不实现具体平台规则**（避免硬编码 process.platform/arch；
/// 规则在宿主 crate 注入，保持本纯逻辑 crate 可单测）。
///
/// trait 方法吃 `&[String]`（资产名列表）返回选中的资产名 + URL（或 None）。
pub trait AssetSelector {
    /// 从 `assets`（资产名列表）里挑出适配当前平台的资产名。None = 无适配（= 上游 `findSuitableAsset → null`）。
    fn select(&self, assets: &[String]) -> Option<String>;
}

/// 简单资产选择器：按子串匹配（测试用，移植纪律：真实平台规则在宿主 crate 注入）。
///
/// 例如 `SubstringSelector::new("linux-amd64")` 会选中含 "linux-amd64" 的资产名。
#[derive(Debug, Clone)]
pub struct SubstringSelector {
    pub needle: String,
}

impl SubstringSelector {
    #[must_use]
    pub fn new(needle: impl Into<String>) -> Self {
        Self {
            needle: needle.into(),
        }
    }
}

impl AssetSelector for SubstringSelector {
    fn select(&self, assets: &[String]) -> Option<String> {
        assets.iter().find(|a| a.contains(&self.needle)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, url: &str, sha: Option<&str>) -> VersionManifestEntry {
        VersionManifestEntry {
            version: version.to_string(),
            url: url.to_string(),
            sha256: sha.map(String::from),
            prerelease: false,
            notes: String::new(),
        }
    }

    #[test]
    fn from_single_json_parses() {
        let json = r#"{"version":"1.14.0","url":"https://x/core","sha256":"abc"}"#;
        // sha256 "abc" 不合法 → 报错。
        let err = VersionManifest::from_single_json(json).unwrap_err();
        assert_eq!(err, ManifestError::InvalidSha256("abc".to_string()));

        let valid = format!(
            r#"{{"version":"1.14.0","url":"https://x/core","sha256":"{}"}}"#,
            "a".repeat(64)
        );
        let m = VersionManifest::from_single_json(&valid).unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].version, "1.14.0");
        assert_eq!(
            m.entries[0].sha256.as_deref(),
            Some("a".repeat(64).as_str())
        );
    }

    #[test]
    fn from_array_json_parses_and_validates() {
        let json = format!(
            r#"[{{"version":"1.13.13","url":"u1"}},{{"version":"1.14.0","url":"u2","sha256":"{}"}}]"#,
            "b".repeat(64)
        );
        let m = VersionManifest::from_array_json(&json).unwrap();
        assert_eq!(m.entries.len(), 2);
        // 第二条有 sha256。
        assert!(m.entries[1].sha256.is_some());
        // 第一条无 sha256（可选字段）。
        assert!(m.entries[0].sha256.is_none());
    }

    #[test]
    fn latest_finds_newest_formal() {
        let m = VersionManifest::from_entries(vec![
            entry("1.13.13", "u1", None),
            entry("1.14.0", "u2", None),
            entry("1.13.15", "u3", None),
        ]);
        // 当前 1.13.13 → 最新应 1.14.0。
        let latest = m.latest("1.13.13", false, None).unwrap().unwrap();
        assert_eq!(latest.version, "1.14.0");
    }

    #[test]
    fn latest_filters_prerelease() {
        let mut pre = entry("1.15.0-beta.1", "u-pre", None);
        pre.prerelease = true;
        let m = VersionManifest::from_entries(vec![entry("1.14.0", "u1", None), pre]);
        // include_prerelease=false → 跳过 beta，返回 1.14.0。
        let latest = m.latest("1.13.0", false, None).unwrap().unwrap();
        assert_eq!(latest.version, "1.14.0");
        // include_prerelease=true → 返回 beta。
        let latest = m.latest("1.13.0", true, None).unwrap().unwrap();
        assert_eq!(latest.version, "1.15.0-beta.1");
    }

    #[test]
    fn latest_cross_band_restriction() {
        let m = VersionManifest::from_entries(vec![
            entry("1.13.14", "u1", None),
            entry("1.14.0", "u2", None), // 跨带（1.13 → 1.14）
        ]);
        // 当前 1.13.13，restrict 同带 → 返回 1.13.14（1.14.0 被硬闸拦）。
        let latest = m.latest("1.13.13", false, Some(true)).unwrap().unwrap();
        assert_eq!(latest.version, "1.13.14");
        // 不 restrict → 返回 1.14.0。
        let latest = m.latest("1.13.13", false, Some(false)).unwrap().unwrap();
        assert_eq!(latest.version, "1.14.0");
    }

    #[test]
    fn latest_none_when_already_newest() {
        let m = VersionManifest::from_entries(vec![entry("1.14.0", "u1", None)]);
        // 当前已是 1.14.0 → 无更新（None）。
        assert!(m.latest("1.14.0", false, None).unwrap().is_none());
        assert!(!m.has_update("1.14.0", false, None).unwrap());
    }

    #[test]
    fn latest_none_when_all_filtered() {
        let m = VersionManifest::from_entries(vec![]);
        // 空清单 → latest 返回 None（不报错，has_update=false）。
        assert!(m.latest("1.0.0", false, None).unwrap().is_none());
    }

    #[test]
    fn entry_validate_rejects_bad_version_and_hash() {
        let bad_ver = VersionManifestEntry {
            version: String::new(),
            url: "u".into(),
            sha256: None,
            prerelease: false,
            notes: String::new(),
        };
        assert_eq!(
            bad_ver.validate(),
            Err(ManifestError::InvalidVersion(String::new()))
        );

        let bad_hash = VersionManifestEntry {
            version: "1.0.0".into(),
            url: "u".into(),
            sha256: Some("tooshort".into()),
            prerelease: false,
            notes: String::new(),
        };
        assert_eq!(
            bad_hash.validate(),
            Err(ManifestError::InvalidSha256("tooshort".into()))
        );
    }

    #[test]
    fn substring_selector_picks_match() {
        let sel = SubstringSelector::new("linux-amd64");
        let assets = vec![
            "sing-box-1.14.0-darwin-arm64.tar.gz".to_string(),
            "sing-box-1.14.0-linux-amd64.tar.gz".to_string(),
            "sing-box-1.14.0-windows-amd64.zip".to_string(),
        ];
        let picked = sel.select(&assets).unwrap();
        assert!(picked.contains("linux-amd64"));

        // 无匹配 → None。
        let sel2 = SubstringSelector::new("freebsd");
        assert!(sel2.select(&assets).is_none());
    }

    #[test]
    fn entry_display() {
        let e = entry("1.14.0", "https://x/core", None);
        assert_eq!(e.to_string(), "v1.14.0 (https://x/core)");

        let mut pre = e;
        pre.prerelease = true;
        assert_eq!(pre.to_string(), "v1.14.0 (https://x/core) [prerelease]");
    }
}
