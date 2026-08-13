//! 核心 staged 更新全周期（移植自 上游 `CoreUpdateService` 的 staged 流）。
//!
//! 移植自三段 Polaris staged 更新逻辑（`CoreUpdateService.ts`）：
//!   - `updateCore:272-380`：手动更新（下载→解压→预检→停代理→备份→替换→重启）。
//!   - `runAutoUpdateCycle:730-838`：自动周期（检查→带内下载+预检+暂存/落位；跨带仅提示）。
//!   - `tryApplyStaged:851-989`：落位（仅在代理进程不存在时真正换核；返回 applied/discarded/deferred/failed/noop）。
//!
//! 全周期：
//! ```text
//!   download(manifest.url) ──▶ verify_bytes(bytes, manifest.sha256) ──▶ atomic_replace_multi(fs, dest_dir, files)
//!                                                                      │
//!                                                          (任一失败 → StagedUpdateError)
//! ```
//!
//! **纯逻辑纪律**：下载经 [`UpdateDownloader`] trait、FS 经 [`UpdateFs`] trait、状态簿记经
//! [`StagedStateStore`] trait，全部可注入 mock。本 crate 不发起真实 HTTP / 不写真实 FS（生产实现由宿主注入）。
//! 预检（sing-box check）/ 停代理 / 重启 / 签名等 runtime 行为**不在本 crate**（超出纯逻辑边界，由宿主 orchestration 层负责）。

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::traits::{DownloadError, UpdateDownloader, UpdateFs};
use crate::verify::{atomic_replace_multi, verify_bytes, VerifyError};
use crate::version::{is_newer, same_major_minor};
use crate::{ManifestError, VersionManifestEntry};

/// staged 更新错误（全周期任一阶段失败的统一形状）。
#[derive(Debug, Error)]
pub enum StagedUpdateError {
    /// 下载失败（= Polaris downloadFile reject）。
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
    /// SHA256 校验失败（manifest 携带 sha256 时；下载后字节与期望不符，防中间人篡改）。
    #[error("verify failed: {0}")]
    Verify(#[from] VerifyError),
    /// manifest 解析/选择失败（= Polaris checkUpdate 的 asset/version 失败路径）。
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    /// FS 操作失败（写临时文件 / 原子 rename 等）。
    #[error("fs error: {0}")]
    Fs(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// 状态簿记失败（StagedStateStore 读写失败；不阻断已落位的核，仅日志）。
    #[error("state store error: {0}")]
    StateStore(String),
    /// 版本回退：要落的版本不比当前新（= 上游 `compareVersions(staged.version, current) <= 0`，作废 staged）。
    #[error("staged version {staged} is not newer than current {current}")]
    NotNewer { staged: String, current: String },
    /// 跨带硬闸：要落的版本跨 major.minor（= Polaris sameMajorMinor 自动路径硬闸；手动可绕过）。
    #[error("staged version {staged} crosses band from current {current}")]
    CrossBand { staged: String, current: String },
}

impl From<std::io::Error> for StagedUpdateError {
    fn from(e: std::io::Error) -> Self {
        Self::Fs(Box::new(e))
    }
}

/// staged 暂存信息（移植自 `core-update-state-store.ts:StagedCoreInfo`）。
///
/// 已下载预检通过、待落位的内核：version + 暂存目录路径 + 暂存时间。
/// Polaris 原结构含 `dir`（userData/core-staged）；本 crate 泛化为字符串路径（不耦合 userData）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedInfo {
    /// 版本号（去 v 前缀）。
    pub version: String,
    /// 暂存目录路径（含解压后的核 + 配套文件）。
    pub dir: PathBuf,
    /// 暂存时间（ISO 8601，移植自 上游 `new Date().toISOString()`）。
    pub staged_at: String,
}

/// staged 状态簿记 trait（移植自 上游 `CoreUpdateStateStore`，`core-update-state-store.ts`）。
///
/// 抽象出「记录/读取 staged 版本」的持久面：生产实现写 JSON 文件（对齐 Polaris 的 core-update-state.json
/// 原子写 tmp+rename）；测试可注入内存 map。本 crate 不关心持久化细节，只定义读写契约。
pub trait StagedStateStore {
    /// 读取当前 staged 信息（None = 无暂存）。
    fn load_staged(&self) -> Option<StagedInfo>;

    /// 写入 staged 信息（= 上游 `saveAutoState({ staged })`）。
    ///
    /// # Errors
    ///
    /// 持久化失败（磁盘满/权限等）；调用方可吞掉（不阻断下载已完成的事实）。
    fn save_staged(&self, info: &StagedInfo) -> Result<(), StagedUpdateError>;

    /// 清除 staged 信息（= 上游 `clearStaged`：删元数据 + 删暂存目录）。
    ///
    /// # Errors
    ///
    /// 持久化失败。
    fn clear_staged(&self) -> Result<(), StagedUpdateError>;

    /// 读取最近成功落位的版本（用于「上一版」展示 / 回滚判断）。
    fn last_applied_version(&self) -> Option<String>;

    /// 记录成功落位的版本（= 上游 `pendingChangeNotice` / `verifiedCeiling` 更新）。
    ///
    /// # Errors
    ///
    /// 持久化失败。
    fn record_applied(&self, version: &str) -> Result<(), StagedUpdateError>;
}

/// 内存状态簿记（测试用）：不持久化，进程内 map。
#[derive(Debug, Default)]
pub struct MemoryStateStore {
    staged: std::cell::RefCell<Option<StagedInfo>>,
    last_applied: std::cell::RefCell<Option<String>>,
}

impl MemoryStateStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl StagedStateStore for MemoryStateStore {
    fn load_staged(&self) -> Option<StagedInfo> {
        self.staged.borrow().clone()
    }

    fn save_staged(&self, info: &StagedInfo) -> Result<(), StagedUpdateError> {
        *self.staged.borrow_mut() = Some(info.clone());
        Ok(())
    }

    fn clear_staged(&self) -> Result<(), StagedUpdateError> {
        *self.staged.borrow_mut() = None;
        Ok(())
    }

    fn last_applied_version(&self) -> Option<String> {
        self.last_applied.borrow().clone()
    }

    fn record_applied(&self, version: &str) -> Result<(), StagedUpdateError> {
        *self.last_applied.borrow_mut() = Some(version.to_string());
        Ok(())
    }
}

/// 落位结果枚举（移植自 上游 `StagedApplyResult`，`CoreUpdateService.ts:66`）。
///
/// 让调用方区分「已落位 / 已作废 / 延后 / 失败 / 无操作」，而非靠「staged 是否还在」二值推断
/// （Polaris 修 M1 的原因：原 void/boolean 返回把 discarded/deferred/failed 一律误判，UI 误报「已应用」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApplyOutcome {
    /// 已换核落位成功（= 上游 `applied`：installCoreFromDir 完成、staged 已清）。
    Applied,
    /// staged 已不适用被作废（= 上游 `discarded`：不再领先 / 跨带 / 缺文件，未换核）。
    Discarded,
    /// 版本或带闸拒绝落位（延后，保持 staged 或不入暂存；= Polaris 部分 deferred 语义）。
    Deferred,
    /// 落位中途异常（= 上游 `failed`：已尽力 restoreBackup，staged 保留待重试）。
    Failed,
    /// 无 staged 可落位 / 重入 → 无操作（= 上游 `noop`）。
    Noop,
}

/// staged 更新周期配置。
#[derive(Debug, Clone)]
pub struct StagedConfig {
    /// 落位目标目录（核 + 配套文件的最终位置）。
    pub dest_dir: PathBuf,
    /// 是否强制兼容版本带（true = 跨 major.minor 拒绝落位，= Polaris 自动路径硬闸；false = 手动路径允许跨带）。
    pub restrict_band: bool,
}

impl StagedConfig {
    #[must_use]
    pub fn new(dest_dir: impl Into<PathBuf>) -> Self {
        Self {
            dest_dir: dest_dir.into(),
            restrict_band: true,
        }
    }

    /// 设为手动路径：允许跨带（= Polaris updateCore 手动更新不受 sameMajorMinor 限制）。
    #[must_use]
    pub fn allow_cross_band(mut self) -> Self {
        self.restrict_band = false;
        self
    }
}

/// 核心 staged 更新器：编排「下载 → SHA256 校验 → 原子替换 → 版本簿记」全周期。
///
/// 移植自 上游 `CoreUpdateService` 的 staged 流（runAutoUpdateCycle + tryApplyStaged + installCoreFromDir），
/// 但剥离 runtime 部分（预检/停代理/重启/签名由宿主层负责），只保留纯逻辑编排。
///
/// 三依赖注入（trait）：
///   - `downloader: &dyn UpdateDownloader`：字节获取（生产 HTTP / 测试 mock）。
///   - `fs: &dyn UpdateFs`：文件落盘 + 原子 rename（生产 std::fs / 测试 tempfile）。
///   - `store: &dyn StagedStateStore`：staged 版本簿记（生产 JSON 文件 / 测试内存 map）。
pub struct CoreStagedUpdater<'a> {
    downloader: &'a dyn UpdateDownloader,
    fs: &'a dyn UpdateFs,
    store: &'a dyn StagedStateStore,
    config: StagedConfig,
}

impl<'a> CoreStagedUpdater<'a> {
    /// 构造：注入三个 trait 依赖 + 目标目录配置。
    #[must_use]
    pub fn new(
        downloader: &'a dyn UpdateDownloader,
        fs: &'a dyn UpdateFs,
        store: &'a dyn StagedStateStore,
        config: StagedConfig,
    ) -> Self {
        Self {
            downloader,
            fs,
            store,
            config,
        }
    }

    /// 全周期：下载 manifest 指向的版本 → SHA256 校验 → 原子替换到 dest_dir → 记录版本。
    ///
    /// 移植自 上游 `CoreUpdateService.updateCore:272-380`（手动）+ `tryApplyStaged:918-981`（落位）的核心步骤，
    /// 剥离预检/停代理/重启。
    ///
    /// 步骤：
    ///   1. 版本闸门：检查 `entry.version` 是否比 `current` 新（否则 [`ApplyOutcome::Discarded`]，= Polaris
    ///      `compareVersions(staged.version, current) <= 0` 作废）。
    ///   2. 带闸门：`restrict_band` 且跨 major.minor → [`ApplyOutcome::Deferred`]（= Polaris 自动路径硬闸）。
    ///   3. 下载：`downloader.download(entry.url)` → bytes（失败 → [`StagedUpdateError::Download`]）。
    ///   4. 校验：`entry.sha256` 存在则 `verify_bytes`（失败 → [`StagedUpdateError::Verify`]；Polaris 原本无此强校验，
    ///      本 crate 增强为防中间人篡改）。
    ///   5. 原子替换：`atomic_replace_multi(fs, dest_dir, [(核名, bytes)])`（失败 → [`StagedUpdateError::Fs`]）。
    ///   6. 簿记：`store.record_applied(version)` + `store.clear_staged()`。
    ///
    /// # Errors
    ///
    /// 下载/校验/FS 失败透传 [`StagedUpdateError`]。版本/带闸门不报错，返回 [`ApplyOutcome::Discarded`] /
    /// [`ApplyOutcome::Deferred`]。
    pub fn apply(
        &self,
        entry: &VersionManifestEntry,
        current: &str,
        core_filename: &str,
    ) -> Result<ApplyOutcome, StagedUpdateError> {
        // 1. 版本闸门：staged 必须比 current 新（= Polaris tryApplyStaged 的 compareVersions <= 0 作废）。
        if !is_newer(&entry.version, current).unwrap_or(false) {
            return Ok(ApplyOutcome::Discarded);
        }
        // 2. 带闸门：restrict 时跨带拒绝（= Polaris sameMajorMinor 自动路径硬闸）。
        if self.config.restrict_band && !same_major_minor(&entry.version, current).unwrap_or(false)
        {
            return Ok(ApplyOutcome::Deferred);
        }

        // 3. 下载。
        let bytes = self.downloader.download(&entry.url)?;

        // 4. SHA256 校验（manifest 携带 hash 时；防中间人篡改，强于 Polaris 的 size 校验）。
        if let Some(expected_hash) = &entry.sha256 {
            verify_bytes(&bytes, expected_hash)?;
        }

        // 5. 原子替换到 dest_dir（tmp→rename 编排，保证 dest_dir 要么全换要么不动）。
        self.fs.create_dir_all(&self.config.dest_dir)?;
        let files = vec![(core_filename.to_string(), bytes)];
        atomic_replace_multi(self.fs, &self.config.dest_dir, &files)?;

        // 6. 簿记：记录成功版本 + 清 staged。
        // 簿记失败不回滚已落位的核（核已是新版，仅日志告警；对齐 Polaris saveAutoState 失败的 best-effort 语义）。
        // 调用方应据日志排查，但不影响更新生效。
        self.store.record_applied(&entry.version)?;
        // 清 staged（落位成功后）。
        let _ = self.store.clear_staged();

        Ok(ApplyOutcome::Applied)
    }

    /// 暂存（不落位）：下载 + 校验后写到 `staged_dir`，记录 StagedInfo。
    ///
    /// 移植自 上游 `stageCore:700-723`（代理运行中时先暂存，延到安全窗口落位）。
    /// 与 [`Self::apply`] 的区别：不替换现役核目录，只把核放到暂存目录待后续 [`Self::try_apply_staged`]。
    ///
    /// # Errors
    ///
    /// 下载/校验/FS 失败透传。版本/带闸门返回 Discarded/Deferred。
    pub fn stage(
        &self,
        entry: &VersionManifestEntry,
        current: &str,
        core_filename: &str,
        staged_dir: &Path,
        staged_at: &str,
    ) -> Result<ApplyOutcome, StagedUpdateError> {
        // 版本 + 带闸门（同 apply）。
        if !is_newer(&entry.version, current).unwrap_or(false) {
            return Ok(ApplyOutcome::Discarded);
        }
        if self.config.restrict_band && !same_major_minor(&entry.version, current).unwrap_or(false)
        {
            return Ok(ApplyOutcome::Deferred);
        }

        // 下载 + 校验。
        let bytes = self.downloader.download(&entry.url)?;
        if let Some(expected_hash) = &entry.sha256 {
            verify_bytes(&bytes, expected_hash)?;
        }

        // 写到暂存目录（清旧暂存后重建，对齐 Polaris stageCore 的 `rmSync + mkdirSync + copyFileSync`）。
        let _ = self.fs.remove_dir_all(staged_dir);
        self.fs.create_dir_all(staged_dir)?;
        let files = vec![(core_filename.to_string(), bytes)];
        atomic_replace_multi(self.fs, staged_dir, &files)?;

        // 簿记：记录 StagedInfo（= Polaris saveAutoState({ staged })）。
        let info = StagedInfo {
            version: entry.version.clone(),
            dir: staged_dir.to_path_buf(),
            staged_at: staged_at.to_string(),
        };
        self.store.save_staged(&info)?;

        Ok(ApplyOutcome::Applied)
    }

    /// 落位已暂存的核：把 staged 目录的文件原子替换到 dest_dir。
    ///
    /// 移植自 上游 `tryApplyStaged:918-981`（仅在代理进程不存在时真正换核）。
    /// **本方法不检查代理状态**（runtime 信号由宿主层 gating）；它只做「staged 目录 → dest_dir」的文件搬移
    /// + 版本闸门 + 簿记。代理运行态的 gating 在宿主层调本方法前判定。
    ///
    /// # Errors
    ///
    /// 版本/带闸门返回 Discarded/Deferred；FS 失败透传（= Polaris failed 分支）。
    pub fn try_apply_staged(
        &self,
        current: &str,
        core_filename: &str,
    ) -> Result<ApplyOutcome, StagedUpdateError> {
        let staged = match self.store.load_staged() {
            Some(s) => s,
            None => return Ok(ApplyOutcome::Noop), // = Polaris 无 staged → noop
        };

        // 版本闸门：staged 不再领先当前 → 作废（= Polaris compareVersions <= 0 → clearStaged → discarded）。
        if !is_newer(&staged.version, current).unwrap_or(false) {
            self.discard_staged()?;
            return Ok(ApplyOutcome::Discarded);
        }
        // 带闸门：restrict 时跨带拒绝（保持 staged，延后；= Polaris deferred 语义之一）。
        if self.config.restrict_band && !same_major_minor(&staged.version, current).unwrap_or(false)
        {
            return Ok(ApplyOutcome::Deferred);
        }

        // 读 staged 文件 → 原子替换到 dest_dir。
        let staged_core = self.fs.join(&staged.dir, core_filename);
        if !self.fs.exists(&staged_core) {
            // staged 核缺失 → 作废清理（= 上游 `staged 暂存核心缺失，清理`）。
            self.discard_staged()?;
            return Ok(ApplyOutcome::Discarded);
        }
        let bytes = self.fs.read(&staged_core)?;
        self.fs.create_dir_all(&self.config.dest_dir)?;
        let files = vec![(core_filename.to_string(), bytes)];
        atomic_replace_multi(self.fs, &self.config.dest_dir, &files)?;

        // 簿记：记录成功 + 清 staged（= Polaris clearStaged + pendingChangeNotice）。
        self.store.record_applied(&staged.version)?;
        self.store.clear_staged()?;

        Ok(ApplyOutcome::Applied)
    }

    /// 作废 staged：清暂存目录 + 清簿记（= 上游 `clearStaged`）。
    ///
    /// # Errors
    ///
    /// 仅簿记失败（目录清理失败被吞，对齐 上游 `try { rmSync } catch {}`）。
    pub fn discard_staged(&self) -> Result<(), StagedUpdateError> {
        if let Some(staged) = self.store.load_staged() {
            // 目录清理失败无害（下次 stage 会重建覆盖）；仅日志。
            let _ = self.fs.remove_dir_all(&staged.dir);
        }
        self.store.clear_staged()
    }

    /// 配置访问器（宿主层读 dest_dir 等）。
    #[must_use]
    pub fn config(&self) -> &StagedConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{MemoryDownload, StdFs};
    use crate::verify::sha256_hex;

    /// 构造一个完整测试 fixture：内存下载器（URL→bytes）+ StdFs（tempfile 沙箱）+ 内存 store。
    struct Fixture {
        dl: MemoryDownload,
        fs: StdFs,
        store: MemoryStateStore,
        tmp: tempfile::TempDir,
    }

    impl Fixture {
        /// 按 `(url, bytes)` 注入下载条目（测试自行决定 URL 与 sha256 是否匹配）。
        fn with_entry(url: &str, bytes: &[u8]) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dl = MemoryDownload::new().with(url, bytes.to_vec());
            Self {
                dl,
                fs: StdFs,
                store: MemoryStateStore::new(),
                tmp,
            }
        }
    }

    /// 便利：用 `(version, bytes)` 推导 URL 与 sha256，构造 entry + fixture 配对。
    /// URL 固定为 `https://x/core-{version}`；hash 由真实 bytes 计算（匹配场景）或外部覆盖（失配场景）。
    fn setup(version: &str, bytes: &[u8]) -> (VersionManifestEntry, Fixture) {
        let url = format!("https://x/core-{version}");
        let f = Fixture::with_entry(&url, bytes);
        let hash = sha256_hex(bytes);
        let entry = make_entry(version, &url, Some(&hash));
        (entry, f)
    }

    /// 同 [`setup`] 但 entry 不带 sha256（测「无强校验」场景 / 跨带手动路径）。
    fn setup_no_hash(version: &str, bytes: &[u8]) -> (VersionManifestEntry, Fixture) {
        let url = format!("https://x/core-{version}");
        let f = Fixture::with_entry(&url, bytes);
        let entry = make_entry(version, &url, None);
        (entry, f)
    }

    fn make_entry(version: &str, url: &str, sha: Option<&str>) -> VersionManifestEntry {
        VersionManifestEntry {
            version: version.to_string(),
            url: url.to_string(),
            sha256: sha.map(String::from),
            prerelease: false,
            notes: String::new(),
        }
    }

    #[test]
    fn apply_full_cycle_success() {
        // 端到端：下载 → 校验 → 原子替换 → 簿记。全绿。
        let core = b"fake-sing-box-binary-1.14.1";
        let (entry, f) = setup("1.14.1", core);
        let dest_dir = f.tmp.path().join("dest");
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let outcome = updater.apply(&entry, "1.14.0", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);

        // dest_dir 下有 sing-box，内容正确。
        let written = f.fs.read(&dest_dir.join("sing-box")).unwrap();
        assert_eq!(written, core);
        // 版本簿记已记录。
        assert_eq!(f.store.last_applied_version(), Some("1.14.1".into()));
        // staged 已清（apply 成功后 clear）。
        assert!(f.store.load_staged().is_none());
    }

    #[test]
    fn apply_verify_failure_rejected() {
        // sha256 不匹配 → Verify 错误，dest_dir 不动。
        let core = b"real-bytes";
        let wrong_hash = sha256_hex(b"different-bytes");
        let url = "https://x/core-mismatch";
        let f = Fixture::with_entry(url, core);
        let dest_dir = f.tmp.path().join("dest");
        let entry = make_entry("1.14.1", url, Some(&wrong_hash));
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let err = updater.apply(&entry, "1.14.0", "sing-box").unwrap_err();
        assert!(matches!(err, StagedUpdateError::Verify(_)));
        // dest_dir 未创建（apply 先校验后建目录，校验失败时目录尚未建）。
        assert!(!f.fs.exists(&dest_dir));
    }

    #[test]
    fn apply_download_failure() {
        // URL 不在 mock map → Download 错误。
        let f = Fixture::with_entry("https://x/real", b"x");
        let dest_dir = f.tmp.path().join("dest");
        let entry = make_entry("1.14.1", "https://missing", None);
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let err = updater.apply(&entry, "1.14.0", "sing-box").unwrap_err();
        assert!(matches!(err, StagedUpdateError::Download(_)));
    }

    #[test]
    fn apply_discards_when_not_newer() {
        // staged 版本不比 current 新 → Discarded（不下载）。
        let f = Fixture::with_entry("https://x", b"x");
        let dest_dir = f.tmp.path().join("dest");
        let entry = make_entry("1.14.0", "https://x", None);
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let outcome = updater.apply(&entry, "1.14.0", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Discarded);
    }

    #[test]
    fn apply_deferred_on_cross_band_restricted() {
        // restrict_band + 跨 major.minor → Deferred（不下载，不报错）。
        let f = Fixture::with_entry("https://x", b"x");
        let dest_dir = f.tmp.path().join("dest");
        let entry = make_entry("1.15.0", "https://x", None);
        let cfg = StagedConfig::new(&dest_dir); // restrict_band = true
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let outcome = updater.apply(&entry, "1.14.0", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Deferred);
    }

    #[test]
    fn apply_allows_cross_band_manual() {
        // 手动路径（restrict_band = false）允许跨带落位。
        let core = b"core-1.15.0";
        let (entry, f) = setup_no_hash("1.15.0", core);
        let dest_dir = f.tmp.path().join("dest");
        let cfg = StagedConfig::new(&dest_dir).allow_cross_band();
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let outcome = updater.apply(&entry, "1.14.0", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
    }

    #[test]
    fn stage_then_try_apply_staged() {
        // 两步：先 stage（暂存），再 try_apply_staged（落位）。
        let core = b"core-1.14.2";
        let (entry, f) = setup("1.14.2", core);
        let dest_dir = f.tmp.path().join("dest");
        let staged_dir = f.tmp.path().join("staged");
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        // 1. 暂存。
        let outcome = updater
            .stage(
                &entry,
                "1.14.0",
                "sing-box",
                &staged_dir,
                "2026-07-15T00:00:00Z",
            )
            .unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert!(f.fs.exists(&staged_dir.join("sing-box")));
        assert!(f.store.load_staged().is_some());

        // 2. 落位（模拟代理已停——本方法不查代理态）。
        let outcome = updater.try_apply_staged("1.14.0", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(f.fs.read(&dest_dir.join("sing-box")).unwrap(), core);
        // staged 已清。
        assert!(f.store.load_staged().is_none());
        assert_eq!(f.store.last_applied_version(), Some("1.14.2".into()));
    }

    #[test]
    fn try_apply_staged_noop_without_staged() {
        // 无 staged → Noop。
        let f = Fixture::with_entry("https://x", b"x");
        let dest_dir = f.tmp.path().join("dest");
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        let outcome = updater.try_apply_staged("1.14.0", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Noop);
    }

    #[test]
    fn try_apply_staged_discards_when_not_newer() {
        // staged 版本已被 current 追上 → 作废（Discarded）。
        // 用同 minor 版本（1.13.14 vs 1.13.13）规避 restrict_band 硬闸，让 stage 真正暂存。
        let core = b"core-1.13.14";
        let (entry, f) = setup_no_hash("1.13.14", core);
        let dest_dir = f.tmp.path().join("dest");
        let staged_dir = f.tmp.path().join("staged");
        let cfg = StagedConfig::new(&dest_dir);
        let updater = CoreStagedUpdater::new(&f.dl, &f.fs, &f.store, cfg);

        // 先暂存 1.13.14（current=1.13.13 时算新，同带放行）。
        updater
            .stage(&entry, "1.13.13", "sing-box", &staged_dir, "t")
            .unwrap();
        assert!(f.store.load_staged().is_some());

        // 落位时 current 已升到 1.13.14 → staged 不再新 → 作废。
        let outcome = updater.try_apply_staged("1.13.14", "sing-box").unwrap();
        assert_eq!(outcome, ApplyOutcome::Discarded);
        assert!(f.store.load_staged().is_none());
        // dest_dir 未写（作废不落位）。
        assert!(!f.fs.exists(&dest_dir.join("sing-box")));
    }

    #[test]
    fn memory_state_store_roundtrip() {
        let s = MemoryStateStore::new();
        assert!(s.load_staged().is_none());
        assert!(s.last_applied_version().is_none());

        let info = StagedInfo {
            version: "1.14.0".into(),
            dir: PathBuf::from("/tmp/staged"),
            staged_at: "2026-07-15T00:00:00Z".into(),
        };
        s.save_staged(&info).unwrap();
        assert_eq!(s.load_staged().unwrap(), info);

        s.record_applied("1.14.0").unwrap();
        assert_eq!(s.last_applied_version(), Some("1.14.0".into()));

        s.clear_staged().unwrap();
        assert!(s.load_staged().is_none());
    }
}
