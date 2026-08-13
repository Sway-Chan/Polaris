//! 更新运行时：把 `polaris-updater` 纯逻辑 crate 装配为持有真实 I/O 的运行时实例。
//!
//! 装配内容：
//!  - **随包内核基线**（`core-manifest.json` 的 `bundledCoreVersion`，编译期嵌入）。
//!  - **活核版本双读法**（[`UpdaterRuntime::read_core_version_line`] / [`UpdaterRuntime::read_core_version`]）
//!    —— **两者失败语义刻意不对称**，见下方「双读法陷阱」。
//!  - **更新状态持久化**（`update-state.json`，原子写 tmp+rename）。
//!  - **mini 更新弹窗会话**（`updater::popup::PopupSession` + Tauri 窗口 transport）。
//!
//! # 双读法陷阱（Polaris issue #150 review F1，**移植时必须保留的不对称**）
//!
//! 上游 `ProxyManager` 有两个读活核版本的函数，失败语义**故意相反**：
//!
//! | 函数 | 探测失败时 | 用途 |
//! |---|---|---|
//! | `getCoreVersion`（`ProxyManager.ts:2889-2916`） | **回落随包基线** | 展示 / 一般比较 |
//! | `getCoreVersionLine`（`:2944-2956`） | **返回 `''`** | **reseed 生效校验专用** |
//!
//! 陷阱：一次 spawn 失败在 `getCoreVersion` 眼里长得**和「活核就是随包版本」一模一样**。
//! 若用它校验 reseed，「重读失败」会被回落值伪装成「换核成功」→ 版本闸门误放行 → 带旧核硬跑退回死循环。
//! 故 `classify_reseed_result` 的 `line_after` **只能**来自「失败置空」的读法。
//! 本模块用两个函数名 + 文档 + 单测（`core_version_readers_are_asymmetric`）把该不对称钉死。
//!
//! # 边界声明（2026-07-18 立，2026-07-29 复核订正）
//!
//! 1. **更新链路已全段接线**（此前本条记「检查侧/安装侧仍不可达、前端检查入口仍禁用」，**已过时**）：
//!    传输 `runtime/http.rs`（reqwest+rustls + `CoreDownloader`，带端到端单测）；检查侧
//!    `crates/updater/src/github.rs`（release JSON→manifest 转换 + 平台资产选择 + `APP_UPDATE_REPO`
//!    / `CORE_UPDATE_REPO` 常量）已移植；前端 `SettingsUpdate.tsx` 的检查按钮是活的
//!    （`checkUpdate` → `update_check` → `update_download` → `update_install` 三段齐）。
//!    `updater::traits::UnavailableDownloader` 现**只作为 trait 契约的映射目标存在**，生产注入的是
//!    `CoreDownloader` ⇒ `HTTP_BACKEND_UNAVAILABLE` 在生产不可达（见 `commands/updater.rs:88`）。
//! 2. **核二进制路径解析的单一真值是 [`crate::runtime::proxy::resolve_core_binary`]**（`pub(crate)`）。
//!    本模块**刻意不复制第二份**（§A3 教训：`RuleResourceManager` 曾有 2 域的 `GITHUB_HOSTS` 本地副本
//!    与 `gh-proxy.ts` 的 5 域漂移，令三级兜底自相矛盾）。走**注入**：[`UpdaterRuntime::with_core_binary`]，
//!    注入点 `main.rs:1293`（此前本条记「待编排者提为 `pub(crate)` 并注入」，该待办**已完成**）。
//!    未注入时（异常启动路径）版本读取如实返回「未知」/空串，**不猜、不谎报**。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use polaris_updater::core_build::{
    classify_core_build, ComparableVersion, CoreBuildKind, CoreOverrideDecision,
};
use polaris_updater::popup::{PopupSession, UpdatePopupState};
use polaris_updater::{decide_core_override, extract_version_token};
use serde::{Deserialize, Serialize};

use crate::runtime::update_popup::TauriPopupTransport;

/// `core-manifest.json` 的编译期嵌入（= 上游 `import coreManifest from '../shared/core-manifest.json'`）。
///
/// `bundledCoreVersion` 是**构建期生成**的常量（随包核版本 = 基线），故编译期嵌入语义正确，
/// 且免去运行期 resource 路径解析的一整类失败。
const CORE_MANIFEST_JSON: &str = include_str!("../../core-manifest.json");

/// 随包内核清单（只取本模块需要的字段）。
#[derive(Debug, Clone, Deserialize)]
struct CoreManifest {
    #[serde(rename = "bundledCoreVersion")]
    bundled_core_version: String,
}

/// 解析编译期嵌入的 `core-manifest.json`，取随包基线版本。
///
/// 解析失败 → 回落空串（调用方据此跳过基线比较；**不 panic**：清单损坏不该让整个 App 起不来）。
fn bundled_core_version() -> String {
    serde_json::from_str::<CoreManifest>(CORE_MANIFEST_JSON)
        .map(|m| m.bundled_core_version)
        .unwrap_or_else(|e| {
            log::error!("core-manifest.json 解析失败 {e}：基线比较将跳过");
            String::new()
        })
}

/// 持久化的更新状态（`<config_dir>/update-state.json`）。
///
/// 移植自 上游 `core-update-state.json`（`core-update-state-store.ts:13-25`）+ App 更新的 skipped 版本。
/// 合并为一个文件：Polaris 分两处（`CoreUpdateStateStore` 与 `UpdateService` 各自持久化），
/// 但二者都是「更新域的用户可见状态」，同生命周期、同读写时机 —— 分文件只是上游的历史产物，无收益。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateStateFile {
    /// 用户「跳过此版本」的 App 版本号（= 上游 `UPDATE_SKIP`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_version: Option<String>,
    /// 上次检查更新时间（epoch ms；= 上游 `lastCheckAt`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check_at: Option<u64>,
    /// 已暂存待落位的内核版本（= 上游 `staged`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedRecord>,
    /// 已提示过的跨带版本（= 上游 `crossBandNotifiedVersion`，一次性提示）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cross_band_notified_version: Option<String>,
    /// 版本变更通知（show→ack；= 上游 `pendingChangeNotice`）。
    ///
    /// 「弹一次非每启」：换核成功时写入，UI banner 展示后调 `core:ackVersionChange` 清除。
    /// Polaris 用它取代了旧的「last-known-version !== current」推断式判定 —— 后者每次启动都重弹。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_change_notice: Option<PendingChangeNotice>,
}

/// staged 暂存记录（= 上游 `StagedCoreInfo`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedRecord {
    pub version: String,
    pub dir: String,
    pub staged_at: String,
}

/// 版本变更通知（= 上游 `pendingChangeNotice`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChangeNotice {
    pub previous_version: String,
    pub current_version: String,
}

/// 更新运行时。
pub struct UpdaterRuntime {
    /// 状态文件路径（`<config_dir>/update-state.json`）。
    state_path: PathBuf,
    /// 随包内核基线版本（编译期自 `core-manifest.json`）。
    bundled_core_version: String,
    /// 核二进制路径（**注入**；None = 未注入，版本读取如实报未知，见模块文档边界 2）。
    core_binary: Mutex<Option<PathBuf>>,
    /// mini 更新弹窗会话（None = 弹窗未开）。
    popup: Mutex<Option<PopupSession<TauriPopupTransport>>>,
    /// 内存态状态缓存（避免每次读盘；写时同步落盘）。
    state: Mutex<UpdateStateFile>,
}

impl UpdaterRuntime {
    /// 装配（`AppRuntime::new` 调用一次）。
    ///
    /// 启动即读一次状态文件；读失败（首次启动/损坏）→ 默认空状态（**不 panic**）。
    #[must_use]
    pub fn new(config_dir: PathBuf) -> Self {
        let state_path = config_dir.join("update-state.json");
        let state = load_state_file(&state_path);
        Self {
            state_path,
            bundled_core_version: bundled_core_version(),
            // 环境逃生门与 proxy.rs 的 resolve_core_binary 共享同一契约（POLARIS_SINGBOX_PATH）；
            // 这不是「复制解析逻辑」，只是读同一个约定的环境变量。完整解析仍归 proxy.rs 单一真值。
            core_binary: Mutex::new(
                std::env::var("POLARIS_SINGBOX_PATH")
                    .ok()
                    .map(PathBuf::from)
                    .filter(|p| p.is_file()),
            ),
            popup: Mutex::new(None),
            state: Mutex::new(state),
        }
    }

    /// 注入核二进制路径（见模块文档边界 2：单一真值在 `proxy.rs`，此处只接收）。
    pub fn with_core_binary(&self, path: PathBuf) {
        if let Ok(mut g) = self.core_binary.lock() {
            *g = Some(path);
        }
    }

    /// 随包内核基线版本（= 上游 `coreManifest.bundledCoreVersion`）。
    #[must_use]
    pub fn bundled_core_version(&self) -> &str {
        &self.bundled_core_version
    }

    // ── 活核版本双读法（**不对称失败语义**，见模块文档「双读法陷阱」）──

    /// 读活核 `sing-box version` 的**原始第一行**；**探测失败返回空串**。
    ///
    /// = 上游 `ProxyManager.getCoreVersionLine`（`:2944-2956`）。
    ///
    /// **这是 [`polaris_updater::classify_reseed_result`] 唯一合法的入参来源**：失败置空 →
    /// `classify_core_build` 视为 `unknown` → reseed 判「未生效」（诚实失败）。
    /// **绝不回落随包基线** —— 那会把「重读失败」伪装成「换核成功」。
    #[must_use]
    pub fn read_core_version_line(&self) -> String {
        let Some(bin) = self.core_binary_path() else {
            // 未注入路径 = 探测不可能成功 → 空串（诚实失败，不猜）。
            return String::new();
        };
        match crate::runtime::win_console::no_console_window(Command::new(&bin).arg("version"))
            .output()
        {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .split('\n')
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
            // 非零退出 / spawn 失败 → 空串（**不回落基线**）。
            Ok(out) => {
                log::warn!("sing-box version 非零退出 {:?}：版本行置空", out.status);
                String::new()
            }
            Err(e) => {
                log::warn!("sing-box version spawn 失败 {e}：版本行置空");
                String::new()
            }
        }
    }

    /// 读活核版本 token；**探测失败回落随包基线**。
    ///
    /// = 上游 `ProxyManager.getCoreVersion`（`:2889-2916`，`catch` 分支返回
    /// `coreManifest.bundledCoreVersion`）。
    ///
    /// # ⚠️ 绝不可用于 reseed 生效校验
    ///
    /// 回落语义使「探测失败」与「活核就是随包版本」不可区分。校验换核是否真生效**必须**用
    /// [`Self::read_core_version_line`]。本函数仅供展示 / 一般比较。
    #[must_use]
    pub fn read_core_version(&self) -> String {
        let line = self.read_core_version_line();
        let tok = extract_version_token(&line);
        if tok.is_empty() {
            // 回落基线（**刻意**与上游一致的陷阱语义；调用点须自觉，见文档）。
            return self.bundled_core_version.clone();
        }
        tok
    }

    /// 活核构建来源判定（= 上游 `getCoreBuild`，`CoreUpdateService.ts:1090`）。
    ///
    /// 喂的是**原始版本行**（含 fork 后缀）——`classify_core_build` 要的正是它。
    #[must_use]
    pub fn core_build_kind(&self) -> CoreBuildKind {
        classify_core_build(&self.read_core_version_line())
    }

    /// 核覆盖决策（= 上游 `decideCoreOverride`）。
    ///
    /// 第 2 参数经 [`ComparableVersion::normalize`] 规范化 —— 由类型强制，B-2 整类 bug 在此不可达
    /// （见 `updater::core_build` 模块文档）。
    #[must_use]
    pub fn decide_core_override_for(&self, core_version_raw: &str) -> CoreOverrideDecision {
        decide_core_override(
            self.core_build_kind(),
            &ComparableVersion::normalize(core_version_raw),
            &self.bundled_core_version,
        )
    }

    fn core_binary_path(&self) -> Option<PathBuf> {
        self.core_binary.lock().ok().and_then(|g| g.clone())
    }

    // ── 状态持久化 ──

    /// 读当前状态（内存缓存快照）。
    #[must_use]
    pub fn state(&self) -> UpdateStateFile {
        self.state.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 改状态并落盘（原子写 tmp+rename；= 上游 `saveAutoState` 的 merge + 原子写）。
    ///
    /// # Errors
    ///
    /// 落盘失败（磁盘满/权限）。**内存态已更新**（对齐上游 best-effort：落盘失败不回滚内存）。
    pub fn mutate_state<F: FnOnce(&mut UpdateStateFile)>(&self, f: F) -> Result<(), String> {
        let snapshot = {
            let mut g = self
                .state
                .lock()
                .map_err(|e| format!("state 锁中毒: {e}"))?;
            f(&mut g);
            g.clone()
        };
        save_state_file(&self.state_path, &snapshot)
    }

    // ── 弹窗会话 ──

    /// 弹窗会话（供 `update_popup` 模块建窗/推状态）。
    #[must_use]
    pub fn popup(&self) -> &Mutex<Option<PopupSession<TauriPopupTransport>>> {
        &self.popup
    }

    /// 当前弹窗状态（= 上游 `UPDATE_POPUP_STATE` 的主→弹窗载荷读取端）。
    ///
    /// 返回 `None` = 弹窗未开（**不编造 idle 态**：弹窗不存在与弹窗处于某态是两回事）。
    #[must_use]
    pub fn popup_state(&self) -> Option<UpdatePopupState> {
        self.popup
            .lock()
            .ok()?
            .as_ref()
            .and_then(|s| s.last_state().cloned())
    }
}

/// 读状态文件；不存在/损坏 → 默认空态（首次启动即此路径）。
fn load_state_file(path: &Path) -> UpdateStateFile {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            // 损坏不该让更新域整个瘫掉；如实告警后按空态继续（用户最多丢一次 skip 记录）。
            log::warn!("update-state.json 解析失败 {e}：按空状态继续");
            UpdateStateFile::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UpdateStateFile::default(),
        Err(e) => {
            log::warn!("update-state.json 读取失败 {e}：按空状态继续");
            UpdateStateFile::default()
        }
    }
}

/// 原子写状态文件（tmp → rename；对齐 上游 `CoreUpdateStateStore` 的原子写）。
///
/// rename 同目录为原子 syscall：崩在半路也不会留下截断的 JSON（读到半截 JSON 会让状态静默归零）。
fn save_state_file(path: &Path, state: &UpdateStateFile) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(state).map_err(|e| format!("序列化更新状态失败: {e}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败 {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)
        .map_err(|e| format!("写临时状态文件失败 {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        // rename 失败 → 清残件，避免 .tmp 堆积。
        let _ = std::fs::remove_file(&tmp);
        format!("原子替换状态文件失败 {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 极简临时目录（用完即删）。
    ///
    /// 刻意**不引 `tempfile`**：src-tauri 的 dev-deps 里没有它，而任务边界是「禁止引入新依赖」；
    /// 本用途只需「唯一目录 + Drop 清理」，stdlib 足够（简约阶梯：stdlib 先于依赖）。
    /// Drop 保证清理 —— 本机 `/tmp` 是 tmpfs（12G），残留会累积。
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            // 唯一性：pid + 单调计数器（同进程内并发测试也不撞）。
            use std::sync::atomic::{AtomicU32, Ordering};
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("polaris-updater-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).expect("建临时测试目录");
            Self(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            // 尽力清理：失败也不 panic（Drop 里 panic 会掩盖真实测试失败）。
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rt(dir: &Path) -> UpdaterRuntime {
        UpdaterRuntime::new(dir.to_path_buf())
    }

    #[test]
    fn bundled_core_version_parses_from_embedded_manifest() {
        // 编译期嵌入的 core-manifest.json 必须解析出非空基线 —— 它是 C6 全部决策的锚。
        // 若清单改名/改结构，这条即刻转红（而非在运行期静默按空基线跑）。
        let v = bundled_core_version();
        assert!(!v.is_empty(), "bundledCoreVersion 不得为空");
        assert!(
            v.starts_with(|c: char| c.is_ascii_digit()),
            "基线应是版本号，实得: {v}"
        );
    }

    #[test]
    fn state_file_roundtrip_is_atomic_and_survives_reload() {
        let tmp = TmpDir::new();
        let r = rt(tmp.path());
        assert!(r.state().skipped_version.is_none());

        r.mutate_state(|s| s.skipped_version = Some("4.2.5".into()))
            .unwrap();
        assert_eq!(r.state().skipped_version.as_deref(), Some("4.2.5"));

        // 新实例重读磁盘 → 状态持久化生效。
        let r2 = rt(tmp.path());
        assert_eq!(r2.state().skipped_version.as_deref(), Some("4.2.5"));
        // 无 .tmp 残件。
        assert!(!tmp.path().join("update-state.json.tmp").exists());
    }

    #[test]
    fn corrupt_state_file_degrades_to_empty_not_panic() {
        let tmp = TmpDir::new();
        std::fs::write(tmp.path().join("update-state.json"), b"{not json").unwrap();
        // 损坏文件不得 panic（更新域瘫掉 ≠ App 起不来）。
        let r = rt(tmp.path());
        assert!(r.state().skipped_version.is_none());
        // 仍可正常写回（覆盖损坏文件）。
        r.mutate_state(|s| s.skipped_version = Some("1.0.0".into()))
            .unwrap();
        assert_eq!(
            rt(tmp.path()).state().skipped_version.as_deref(),
            Some("1.0.0")
        );
    }

    #[test]
    fn pending_change_notice_show_then_ack_clears_once() {
        // 「弹一次非每启」：写入 → 读到 → ack 清除 → 再读为空。
        let tmp = TmpDir::new();
        let r = rt(tmp.path());
        r.mutate_state(|s| {
            s.pending_change_notice = Some(PendingChangeNotice {
                previous_version: "1.13.13".into(),
                current_version: "1.14.0".into(),
            });
        })
        .unwrap();
        assert!(r.state().pending_change_notice.is_some());

        r.mutate_state(|s| s.pending_change_notice = None).unwrap();
        assert!(r.state().pending_change_notice.is_none());
        // 重启后仍为空（不复活 → 不会每次启动重弹）。
        assert!(rt(tmp.path()).state().pending_change_notice.is_none());
    }

    #[test]
    fn core_version_readers_are_asymmetric_the_150_f1_trap() {
        // **本仓最要紧的一条不变式**（Polaris issue #150 review F1）：
        //   read_core_version_line 探测失败 → ""（诚实失败）
        //   read_core_version      探测失败 → 回落随包基线（会伪装成「活核=基线」）
        // 二者若同语义，reseed 校验就会把「重读失败」当成「换核成功」→ 带旧核硬跑退回死循环。
        let tmp = TmpDir::new();
        let r = rt(tmp.path());
        // 未注入核路径 = 探测必失败（本机不 spawn 真核，符合「真实安装核不在本机跑」纪律）。
        assert!(r.core_binary_path().is_none());

        assert_eq!(
            r.read_core_version_line(),
            "",
            "失败置空的读法必须返回空串——它是 classify_reseed_result 的唯一合法入参来源"
        );
        assert_eq!(
            r.read_core_version(),
            r.bundled_core_version(),
            "回落基线的读法必须回落基线（刻意保留的上游陷阱语义）"
        );
        // 两者**必须不同**——这正是不对称本身。
        assert_ne!(
            r.read_core_version_line(),
            r.read_core_version(),
            "双读法失败语义一旦对称，#150 F1 的防线即失效"
        );
    }

    #[test]
    fn core_build_kind_unknown_when_probe_fails() {
        // 探测失败 → 版本行空 → classify_core_build 视为 unknown（**不硬判 fork**、更不判 official）。
        let tmp = TmpDir::new();
        let r = rt(tmp.path());
        assert_eq!(r.core_build_kind(), CoreBuildKind::Unknown);
    }

    #[test]
    fn decide_core_override_never_reseeds_when_probe_fails() {
        // 失败安全：探测不到活核 → unknown → 绝不 reseed（不覆盖用户的核）。
        let tmp = TmpDir::new();
        let r = rt(tmp.path());
        let d = r.decide_core_override_for("1.13.13");
        assert!(!d.reseed, "unknown 构建绝不 reseed");
        assert!(d.warn, "旧于基线的 unknown 核应 warn");
    }
}
