//! polaris-updater — 应用自更新 + 核心 staged 更新全周期（纯逻辑）。
//!
//! 见 `~/docs/polaris/design/polaris-system-design.md` §B.2（crate 边界 + 职责 + Polaris 锚点）。
//!
//! ## 移植来源（Polaris TS → Rust 纯逻辑）
//!
//! - [`state`]：4 态弹窗更新 UI 状态机（Checking/Ready/Verifying/Installing/Error）——
//!   移植自 `UpdateService.ts` 的 `UpdateProgress.status` + `UpdatePopupState.phase`
//!   （`src/main/services/UpdateService.ts:77-81, 505-507, 591-644`）。
//! - [`version`]：semver 比较 + 兼容版本带判定——移植自 `shared/version.ts`
//!   （`compareSemver` / `encodeMajorMinor` / `sameMajorMinor`，单一权威）。
//! - [`manifest`]：版本清单解析（最新版本号/下载 URL/SHA256）——移植自
//!   `CoreUpdateService.checkUpdate` + `UpdateService.checkForUpdate` 的 release 解析
//!   （`CoreUpdateService.ts:152-267` / `UpdateService.ts:151-245`）。
//! - [`verify`]：SHA256 校验纯逻辑 + tmp→rename 原子替换编排——移植自
//!   `shared/file-hash.ts` 的 `sha256File` + `CoreUpdateService.installCoreFromDir`
//!   的非 Windows staged-rename 落位（`CoreUpdateService.ts:408-523, 453-479`）。
//! - [`staged`]：[`CoreStagedUpdater`] staged 更新全周期（下载→校验→原子替换→版本簿记）——
//!   移植自 `CoreUpdateService.runAutoUpdateCycle` + `updateCore` + `tryApplyStaged`
//!   （`CoreUpdateService.ts:272-380, 730-838, 851-989`）。
//! - [`scheduler`]：[`UpdateScheduler`] 检查更新调度（定时/手动）——移植自
//!   `CoreUpdateScheduler`（`src/main/services/CoreUpdateScheduler.ts`，STARTUP_DELAY/TICK/due）。
//!
//! ## 纯逻辑纪律（不触碰宿主网络/FS）
//!
//! 下载与文件系统操作经 trait 抽象（[`traits::UpdateDownloader`] / [`traits::UpdateFs`]），
//! 测试注入 mock 即可覆盖全周期，**绝不在本 crate 内发起真实 HTTP 或写真实 FS**。
//! SHA256 校验、原子 rename 编排、版本比较均为纯函数，可直接单测。

#![forbid(unsafe_code)]
// clippy 零警告（对齐移植纪律）：workspace 无集中 [workspace.lints]，与同级 crate（helper-linux /
// net-stack 等）同口径——只 forbid(unsafe_code)，clippy::all 默认级零警告即达标（不 escalate 到
// pedantic/nursery，避免与同级 crate 的 lint 严格度漂移）。

pub mod core_build;
pub mod github;
pub mod manifest;
pub mod popup;
pub mod scheduler;
pub mod staged;
pub mod state;
pub mod traits;
pub mod verify;
pub mod version;

pub use core_build::{
    classify_core_build, classify_reseed_result, decide_core_override, extract_version_token,
    parse_uploaded_core_version, reseed_applied, ComparableVersion, CoreBuildKind,
    CoreOverrideDecision, ReseedResult, UploadedCoreVersion,
};
pub use github::{
    check_app_update, find_suitable_singbox_asset, find_suitable_update_asset,
    github_releases_api_url, parse_asset_digest, AppUpdateCheck, AppUpdateInfo, AssetArch,
    AssetPlatform, GithubAsset, GithubRelease, APP_UPDATE_REPO, CORE_UPDATE_REPO,
};
pub use manifest::{AssetSelector, ManifestError, VersionManifest, VersionManifestEntry};
pub use popup::{
    popup_height_for, PopupAction, PopupBootstrap, PopupSession, PopupTransport, UpdatePopupState,
    DONE_AUTO_CLOSE_MS, POPUP_WIDTH,
};
pub use scheduler::{ScheduleConfig, UpdateScheduler};
pub use staged::{
    ApplyOutcome, CoreStagedUpdater, StagedInfo, StagedStateStore, StagedUpdateError,
};
pub use state::{PopupPhase, UpdateState, UpdateStateError};
pub use traits::{MemoryDownload, UnavailableDownloader, UpdateDownloader, UpdateFs};
pub use verify::{
    atomic_replace, atomic_replace_multi, sha256_hex, sha256_hex_lower, verify_bytes, verify_hex,
    VerifyError,
};
pub use version::{encode_major_minor, same_major_minor, ParseVersionError};
