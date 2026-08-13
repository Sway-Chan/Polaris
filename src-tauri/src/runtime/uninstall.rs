//! 完全卸载编排（上游 `APP_UNINSTALL_ALL`）：提权 helper / 受保护目录内核 / 用户配置 / 应用本体。
//!
//! # 本模块存在的理由：把「顺序、失败传播、部分成功」从不可测的薄壳里拆出来
//!
//! 卸载这条链上真正会出错的不是某一次 `remove_dir_all`，而是**编排**：谁先谁后、某一步炸了之后
//! 还敢不敢继续删、删了一半怎么如实交代。这些恰恰是最难在真机上验的（真跑一次就把被测的安装
//! 删了），所以全部收在本模块的**纯函数**里：
//!
//! - [`run_uninstall`]：固定因果序 + fail-fast，`UninstallOps` 注入 ⇒ 单测断言顺序与失败传播；
//! - [`stop_core_outcome`] / [`verdict_of`] / [`plan_app_removal`] / [`validate_config_dir`]：纯判定，真值表可穷举；
//! - [`SystemUninstallOps`]：唯一碰真实文件系统与提权通道的**最外层薄壳**，且它自己的两条删除腿
//!   也走「先判定后删除」，判定部分仍是纯函数。
//!
//! # 为什么是这个顺序（每一步都有因果，不是随手排的）
//!
//! | # | 步骤 | 为什么必须在这个位置 |
//! |---|------|---------------------|
//! | 0 | [停核](UninstallStep::StopCore) | 受管核跑着 TUN 时删 helper，核就成了用户态杀不动的 root 孤儿 + 全网断（判据复用 [`decide_uninstall_preflight`]） |
//! | 1 | [取消开机自启](UninstallStep::Autostart) | 全链**最便宜、最可逆、零提权**的一步，排最前 ⇒ 失败时一个字节都还没删。放最后则意味着「什么都删完了才发现登录项摘不掉」，而系统此后每次登录都会去拉一个已不存在的可执行文件 |
//! | 2 | [卸 helper](UninstallStep::Helper) | 必须**早于**删用户配置：[`HelperRuntime::uninstall`](crate::runtime::helper::HelperRuntime::uninstall) 把提权脚本写进**配置目录**（`manager.uninstall(&self.dir, …)`）、并从那里读 app 侧 token。先删配置 ⇒ 提权脚本没地方落、token 没得读 ⇒ helper 永远卸不掉 |
//! | 3 | [删用户配置](UninstallStep::UserConfig) | 含**可写内核** `core_update/`、日志 `logs/`、图标缓存 `icons/`、`update-state.json`（受保护目录里那份 root 核由第 2 步的提权脚本删）。放在 helper 之后见上一行 |
//! | 4 | [删更新缓存](UninstallStep::CacheDir) | `app_cache_dir()/updates`（下载的安装包）**在配置目录之外**，删配置带不走 —— 漏掉就是卸载完还剩几百 MB |
//! | 5 | [清 Preferences 域](UninstallStep::Preferences) | macOS `~/Library/Preferences/<identifier>.plist`（[`crate::app_language`] 写的 `AppleLanguages`）**在配置目录之外**。排在这里而不是更早：本进程仍在跑，AppKit 退出前还可能往同一个域写窗口状态等键，越晚清窗口越小（**不能保证零回写**，如实记）。仍在删应用本体之前 —— 那一步之后就没有代码可执行了 |
//! | 6 | [删应用本体](UninstallStep::AppBundle) | 必须**最后**：它是当前正在跑的这个进程的载体。先删它，后面几步就没有代码可执行了 |
//!
//! 「属于 Polaris 的落盘位置」是逐处对过的：`logs/`、`icons/`、`rule-resource/`、`rules/`、
//! `singbox-dashboard/`、`core_update/`、`core-staged/`、`config.json`、`update-state.json`、
//! `helper-client.token` **全在配置目录内**（第 3 步一并带走）；配置目录**之外**只有三处 ——
//! 开机自启登录项（第 1 步）、更新包缓存（第 4 步）、macOS 的应用 Preferences 域（第 5 步），
//! 故它们各占一个独立步骤。
//!
//! # Preferences 域为什么**不能**用 `remove_file`（这一步与其它删除腿形制不同的唯一理由）
//!
//! macOS 的 `~/Library/Preferences/*.plist` 不归进程直接管：`cfprefsd`（Defaults Server）把域
//! 缓存在内存里，绕过它删文件的结果是「删了，然后被守护进程按它的缓存写回来」——
//! 苹果自己的指引与社区实测都是这一条（用 `defaults` / `CFPreferences` / `NSUserDefaults`，
//! 别碰文件）。故本步骤走 [`NSUserDefaults::removePersistentDomainForName`][rm]
//! （= `defaults delete <domain>` 的代码等价形式，与 [`crate::app_language::write_apple_languages`]
//! 写入时同一条通道），**一次 `remove_file` 都不做** —— 也因此它不走 [`validate_removable`]
//! 那套路径白名单，改用 [`validate_pref_domain`] 守域名。
//!
//! [rm]: https://developer.apple.com/documentation/foundation/nsuserdefaults/removepersistentdomain(forname:)
//!
//! 受保护目录里的 root 内核**没有独立步骤**，因为它没有独立的删除通道：`crates/helper-proto`
//! 里根本不存在「删内核 / 删受保护目录」这个 IPC 动词（只有 `InstallCore`），三平台的清除一律
//! 由 `helper-client` 那把 root 卸载脚本顺手做掉（mac `rm -rf /Library/Application Support/Polaris`、
//! linux `rm -rf /usr/local/lib/polaris` 等、win `Remove-Item -Recurse C:\ProgramData\Polaris`）。
//! 故它作为第 1 步的**子结果**如实呈现（带真实路径），而不是伪造成一个「已删除」的独立条目。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::runtime::helper::UninstallPreflight;
use crate::runtime::update_install::mac_app_bundle_from_exe;

/// 用户配置目录的**固定叶名**——白名单判定的锚（`<app_config_dir>/polaris`，见 `main.rs::init_base_dir`）。
///
/// 删除腿只认这个叶名：路径不是本进程算出来的那一个（比如被改成了 `$HOME`）就直接拒绝，
/// 而不是「先删了再说」。
pub const CONFIG_DIR_LEAF: &str = "polaris";

/// 更新包缓存子目录的固定叶名（`app_cache_dir()/updates`，见 `commands::update_download`）。
pub const CACHE_UPDATES_LEAF: &str = "updates";

// ────────────────────────────────────────────────────────────────────────────
// 步骤 / 结果 / 报告（前端逐项呈现的契约面）
// ────────────────────────────────────────────────────────────────────────────

/// 完全卸载的四类目标。**声明序 = 因果序**（理由见模块文档的表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UninstallStep {
    /// 零提权停掉「经 helper 起的」受管核。
    StopCore,
    /// 取消开机自启注册（OS 级登录项，**在配置目录之外**）。
    Autostart,
    /// 卸载提权 helper —— 其 root 脚本同时清掉受保护目录中的内核。
    Helper,
    /// 删用户配置目录（含订阅 / 规则 / 可写内核 / 日志 / 图标缓存）。
    UserConfig,
    /// 删缓存目录中的更新包（`app_cache_dir()/updates`，**在配置目录之外**）。
    CacheDir,
    /// 清应用的 UserDefaults 域（macOS `~/Library/Preferences/<identifier>.plist`，**在配置目录之外**）。
    Preferences,
    /// 删应用本体（最后一步）。
    AppBundle,
}

impl UninstallStep {
    /// **删除腿**的固定执行序。停核不在内：它是前置条件，不是删除动作。
    ///
    /// # 为什么取消自启排在最前
    ///
    /// 它是全链**最便宜、最可逆、且零提权**的一步：失败时一个字节都还没删，用户重试的代价为零。
    /// 反过来，把它放在最后就意味着「helper 已卸、配置已删、应用已删」之后才发现登录项摘不掉 ——
    /// 而那正是后果最重的一项：系统此后每次登录都会去拉一个**已经不存在的可执行文件**。
    /// 它也必须在删应用本体**之前**：注销登录项要读当前 exe 路径，应用没了就无从注销。
    pub const DELETE_ORDER: [Self; 6] = [
        Self::Autostart,
        Self::Helper,
        Self::UserConfig,
        Self::CacheDir,
        Self::Preferences,
        Self::AppBundle,
    ];

    /// 人话步骤名（写进「因上一步失败而未执行」的理由里，日志/UI 都能对上账）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::StopCore => "停止内核",
            Self::Autostart => "取消开机自启",
            Self::Helper => "卸载提权助手",
            Self::UserConfig => "删除用户配置",
            Self::CacheDir => "删除更新缓存",
            Self::Preferences => "清除应用偏好域",
            Self::AppBundle => "删除应用本体",
        }
    }
}

/// 单步结果。**五态而非布尔**——「没做」有三种完全不同的成因，糊成一个 `false` 就等于说谎。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StepOutcome {
    /// 真做了且成功。`detail` 必须说清**动了哪个路径**。
    Done { detail: String },
    /// 本来就没有可做的（helper 没装 / 配置目录不存在）——不算失败，也不算成功。
    Skipped { detail: String },
    /// 本平台/本安装形态**做不到**。如实标注，绝不冒充 `Done`。
    Unsupported { detail: String },
    /// 试了，失败了。`detail` = 失败原因。
    Failed { detail: String },
    /// **因前一步失败而根本没试**（fail-fast 的证据）。
    NotAttempted { detail: String },
}

impl StepOutcome {
    /// 成功。
    pub fn done(detail: impl Into<String>) -> Self {
        Self::Done {
            detail: detail.into(),
        }
    }
    /// 无事可做。
    pub fn skipped(detail: impl Into<String>) -> Self {
        Self::Skipped {
            detail: detail.into(),
        }
    }
    /// 本平台做不到。
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::Unsupported {
            detail: detail.into(),
        }
    }
    /// 失败。
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::Failed {
            detail: detail.into(),
        }
    }

    /// 是否为失败态（**唯一**会触发 fail-fast 的形态）。
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// 一步的完整记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReport {
    pub step: UninstallStep,
    pub outcome: StepOutcome,
}

/// 整体判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UninstallVerdict {
    /// 每一步要么做成了、要么本就无事可做 —— **只有这一态才算卸载成功**。
    Complete,
    /// 没有失败，但有本平台做不到的步骤（典型：Windows 应用本体）⇒ 需要用户手动补完。
    Incomplete,
    /// 有步骤失败（以及因此未执行的后续步骤）。
    Failed,
}

/// 逐项卸载报告（前端据此逐条渲染，**不是**一句「已卸载」）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallReport {
    pub steps: Vec<StepReport>,
    pub verdict: UninstallVerdict,
    /// 用户配置或应用本体已被真删 ⇒ 当前进程所依赖的东西已经没了，应引导退出。
    pub requires_exit: bool,
}

/// 纯判定：逐项结果 → 整体判定。
///
/// 顺序不可换：`Failed`/`NotAttempted` 压过 `Unsupported`，`Unsupported` 压过全绿。
/// **`Skipped` 不降级** —— helper 本就没装，不该把一次干净的卸载判成「不完整」。
#[must_use]
pub fn verdict_of(steps: &[StepReport]) -> UninstallVerdict {
    if steps.iter().any(|s| {
        matches!(
            s.outcome,
            StepOutcome::Failed { .. } | StepOutcome::NotAttempted { .. }
        )
    }) {
        return UninstallVerdict::Failed;
    }
    if steps
        .iter()
        .any(|s| matches!(s.outcome, StepOutcome::Unsupported { .. }))
    {
        return UninstallVerdict::Incomplete;
    }
    UninstallVerdict::Complete
}

/// 纯判定：删过用户配置或应用本体 ⇒ 该退出了。
#[must_use]
pub fn requires_exit_of(steps: &[StepReport]) -> bool {
    steps.iter().any(|s| {
        matches!(s.step, UninstallStep::UserConfig | UninstallStep::AppBundle)
            && matches!(s.outcome, StepOutcome::Done { .. })
    })
}

// ────────────────────────────────────────────────────────────────────────────
// 停核腿 → 步骤结果
// ────────────────────────────────────────────────────────────────────────────

/// 纯映射：停核前置判定 + 停核结果 → [`StepOutcome`]。
///
/// # 停核失败在「完全卸载」里必须是 `Failed`（与 helper 单卸载**刻意相反**）
///
/// `commands::helper_uninstall` 那条腿停不掉核也**继续卸载**，理由成文在
/// [`uninstall_preflight_stop`](crate::runtime::helper::uninstall_preflight_stop)：卸载是用户要的终态，
/// 中止的话「既没卸成、也没停成」更糟，而且**应用还在**，用户还能再点一次、还能 forceKill。
///
/// 完全卸载没有这个兜底：后面三步会依次删掉 helper、删掉配置、删掉**应用本体**。若此时还有一个
/// root 受管核占着 TUN，终局是「一个用户态杀不动的 root 核 + 没有应用 + 没有配置」——用户的网断了，
/// 而能停它的那个程序刚被自己删掉。故这里停不掉就**一步都不删**，如实报错让用户重试。
#[must_use]
pub fn stop_core_outcome(preflight: UninstallPreflight, stop_error: Option<&str>) -> StepOutcome {
    match preflight {
        UninstallPreflight::ProceedDirectly => StepOutcome::skipped(
            "无需停核：代理未运行，或内核不是经提权助手启动的（不归 helper 管，卸载不会让它变孤儿）",
        ),
        UninstallPreflight::StopCoreFirst => match stop_error {
            None => StepOutcome::done("已零提权停止经提权助手启动的受管内核"),
            Some(e) => StepOutcome::failed(format!(
                "停止受管内核失败（{e}）：完全卸载已中止，一项都未删除 —— \
                 继续删下去会留下一个用户态杀不动的 root 内核占着 TUN，而能停它的应用刚好被删掉"
            )),
        },
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 删除前的路径判定（白名单式，**先判定后删除**）
// ────────────────────────────────────────────────────────────────────────────

/// 目标形态：删目录树还是删单文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Dir,
    File,
}

/// 路径被拒的原因（每一条都对应一个具体的误删场景）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathReject {
    /// 相对路径 —— 会相对当前工作目录解析，删到哪儿全看进程 cwd。
    NotAbsolute,
    /// 叶名不在白名单里 —— 路径已经不是本进程算出来的那一个了。
    LeafMismatch,
    /// 太浅（没有具名父目录）：`/polaris`、`C:\polaris` 这种一删就是半个系统。
    TooShallow,
    /// 目标不存在。
    Missing,
    /// 目标是软链 —— 跟着删会删到链外的任意位置。
    Symlink,
    /// 目标形态不符（要目录给了文件，或反之）。
    KindMismatch,
}

impl PathReject {
    /// 人话原因（进报告，用户看得懂为什么没删）。
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotAbsolute => "路径不是绝对路径（会相对进程工作目录解析）",
            Self::LeafMismatch => "路径末段不是 Polaris 自有目录名（白名单不匹配）",
            Self::TooShallow => "路径过浅（没有具名父目录），拒绝删除",
            Self::Missing => "目标不存在",
            Self::Symlink => "目标是软链接（跟随删除会删到链接之外的位置）",
            Self::KindMismatch => "目标形态不符（期望目录/文件不一致）",
        }
    }
}

/// 删除前的白名单式判定。**任一条不满足即拒绝，绝不「先删了再说」**。
///
/// 判据全部来自本进程自己算出来的路径（`app_config_dir` / `current_exe` / `$APPIMAGE`），
/// **没有任何一段来自前端入参**——`app_uninstall_all` 是零参数命令，这是结构性保证而非约定。
///
/// # 变异探针
///
/// 删掉 `is_absolute` 判定 ⇒ [`tests::reject_relative_path`] 转红；删掉叶名判定 ⇒
/// [`tests::reject_leaf_mismatch`] 转红；删掉 `parent` 判定 ⇒ [`tests::reject_too_shallow`] 转红；
/// 把 `symlink_metadata` 换成 `metadata` ⇒ [`tests::reject_symlinked_dir`] 转红。
fn validate_removable(
    path: &Path,
    leaf_ok: &dyn Fn(&str) -> bool,
    want: TargetKind,
) -> Result<(), PathReject> {
    if !path.is_absolute() {
        return Err(PathReject::NotAbsolute);
    }
    let Some(leaf) = path.file_name().and_then(|s| s.to_str()) else {
        return Err(PathReject::LeafMismatch);
    };
    if !leaf_ok(leaf) {
        return Err(PathReject::LeafMismatch);
    }
    // 必须有**具名**父目录：挡掉 `/polaris`、`C:\polaris` 这类一层路径。
    if path.parent().and_then(Path::file_name).is_none() {
        return Err(PathReject::TooShallow);
    }
    // `symlink_metadata` 而非 `metadata`：后者会跟随软链，于是「是不是目录」问的是**链尾**，
    // 而 `remove_dir_all` 删的是链本身/链尾，两者对不上就是任意位置删除。
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(PathReject::Missing),
        Err(_) => return Err(PathReject::Missing),
    };
    if md.file_type().is_symlink() {
        return Err(PathReject::Symlink);
    }
    match want {
        TargetKind::Dir if !md.is_dir() => Err(PathReject::KindMismatch),
        TargetKind::File if !md.is_file() => Err(PathReject::KindMismatch),
        _ => Ok(()),
    }
}

/// 用户配置目录判定：叶名必须**恰好**是 [`CONFIG_DIR_LEAF`]。
pub fn validate_config_dir(path: &Path) -> Result<(), PathReject> {
    validate_removable(path, &|leaf| leaf == CONFIG_DIR_LEAF, TargetKind::Dir)
}

/// 更新缓存目录判定：叶名必须**恰好**是 [`CACHE_UPDATES_LEAF`]。
///
/// 只删这一个子目录、**不删整个 `app_cache_dir()`**：那是 OS 给的应用缓存根，Polaris 在其中
/// 唯一的写入点就是 `updates/`（`commands/updater.rs` 的下载腿）。整根删掉等于替 OS 和将来
/// 可能出现的其它写入者做主，收益为零、风险不为零。
pub fn validate_cache_updates_dir(path: &Path) -> Result<(), PathReject> {
    validate_removable(path, &|leaf| leaf == CACHE_UPDATES_LEAF, TargetKind::Dir)
}

// ────────────────────────────────────────────────────────────────────────────
// Preferences 域：域名判定（纯函数；这一步没有路径可判，判的是**域名**）
// ────────────────────────────────────────────────────────────────────────────

/// UserDefaults 域名被拒的原因。每一条都对应一个具体的误清场景。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefDomainReject {
    /// 空 / 全空白 —— 取不到 identifier。传给 `removePersistentDomainForName:` 是未定义行为面。
    Empty,
    /// 命中系统**全局**域。清它等于把用户全系统的偏好（语言、区域、键盘、滚动方向…）一把抹掉，
    /// 而那与 Polaris 毫无关系 —— 这是本判定存在的首要理由。
    Global,
    /// 不是反向 DNS 形态（无 `.`，或含路径分隔符/空白）—— identifier 已经不是本应用那一个了。
    Malformed,
}

impl PrefDomainReject {
    /// 人话原因（进报告，用户看得懂为什么没清）。
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "应用 identifier 为空，算不出 UserDefaults 域名",
            Self::Global => "该域是系统全局偏好域（清除它会抹掉与 Polaris 无关的全系统设置）",
            Self::Malformed => {
                "域名不是应用 identifier 的反向 DNS 形态（含路径分隔符/空白，或没有点号）"
            }
        }
    }
}

/// 系统全局偏好域的各种写法。`removePersistentDomainForName:` 收到它们会清掉
/// `~/Library/Preferences/.GlobalPreferences.plist` —— 用户全系统的语言/区域/键盘设置。
const GLOBAL_PREF_DOMAINS: [&str; 3] = [
    "NSGlobalDomain",
    ".GlobalPreferences",
    "kCFPreferencesAnyApplication",
];

/// 清 UserDefaults 域之前的白名单式判定。**任一条不满足即拒绝，绝不「先清了再说」**。
///
/// 判据来自 `tauri.conf.json` 的 `identifier`（编译期常量，不是前端入参），与其它删除腿同一条纪律：
/// 判定不看「值从哪来」，只看「值长什么样」——来源哪天变了，这道判定仍在。
///
/// # 变异探针
///
/// 删掉 `GLOBAL_PREF_DOMAINS` 判据 ⇒ [`tests::reject_global_pref_domains`] 转红；
/// 删掉 `contains('.')` 判据 ⇒ [`tests::reject_malformed_pref_domain`] 转红。
pub fn validate_pref_domain(identifier: &str) -> Result<(), PrefDomainReject> {
    let id = identifier.trim();
    if id.is_empty() {
        return Err(PrefDomainReject::Empty);
    }
    if GLOBAL_PREF_DOMAINS
        .iter()
        .any(|g| g.eq_ignore_ascii_case(id))
    {
        return Err(PrefDomainReject::Global);
    }
    // 反向 DNS 形态：必须有点号（`polaris` 这种裸名在 defaults 里同样能建域，但它不是本应用的域），
    // 且不得含路径分隔符或空白（那说明拿到的根本不是 identifier）。
    if !id.contains('.')
        || id.starts_with('.')
        || id.contains('/')
        || id.contains('\\')
        || id.chars().any(char::is_whitespace)
    {
        return Err(PrefDomainReject::Malformed);
    }
    Ok(())
}

/// 应用 Preferences 域的落盘路径（macOS 非沙盒形态：`$HOME/Library/Preferences/<identifier>.plist`）。
///
/// **只用于报告文案**：真正的清除走 `removePersistentDomainForName:`（理由见模块文档），
/// 本函数算出来的路径一个字节都不会被删。写成纯函数是为了让它可测 —— 拼错的形态是
/// 「报告里指了个不存在的文件」，用户照着去看会以为没清干净。
#[cfg(any(target_os = "macos", test))]
#[must_use]
pub fn preferences_plist_path(home: &Path, identifier: &str) -> PathBuf {
    home.join("Library")
        .join("Preferences")
        .join(format!("{identifier}.plist"))
}

/// macOS `.app` 包判定：叶名必须以 `.app` 结尾且是真目录。
pub fn validate_app_bundle(path: &Path) -> Result<(), PathReject> {
    validate_removable(path, &|leaf| leaf.ends_with(".app"), TargetKind::Dir)
}

/// Linux AppImage 判定：叶名必须以 `.AppImage` 结尾（忽略大小写）且是真文件。
pub fn validate_appimage(path: &Path) -> Result<(), PathReject> {
    validate_removable(
        path,
        &|leaf| leaf.to_ascii_lowercase().ends_with(".appimage"),
        TargetKind::File,
    )
}

// ────────────────────────────────────────────────────────────────────────────
// 应用本体：三平台可行性判定（纯函数）
// ────────────────────────────────────────────────────────────────────────────

/// 删除应用本体的计划。**`Unsupported` 是一等公民**——做不到就如实说，不假装做了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRemoval {
    /// 删整个目录树（macOS `.app` 包）。
    RemoveDir(PathBuf),
    /// 删单个文件（Linux AppImage）。
    RemoveFile(PathBuf),
    /// 拉起系统卸载程序（Windows NSIS `uninstall.exe`）。**这不等于「已删除」**，报告里必须区分。
    LaunchUninstaller(PathBuf),
    /// 本平台/本安装形态做不到，附**用户能照做的**手动路径。
    Unsupported(String),
}

/// 纯判定：当前平台 + 安装形态 ⇒ 应用本体怎么删（或为什么删不了）。
///
/// `exists` 注入（而非直接 `Path::exists`）是为了让 **Windows 腿在 Linux 开发机上可测** ——
/// 否则那条分支只能靠读代码推理，等于没门。
///
/// # 三平台的实情（每条都对应一个真实的系统约束，不是偷懒）
///
/// - **macOS**：`.app` 是自包含目录树，且 Unix 允许删掉正在运行的可执行文件所在的目录
///   （进程持 inode，删的是目录项）⇒ 可行。定位不到 `.app`（开发构建 / 裸二进制）就**不猜路径**，
///   与 `update_install` 里「定位不到 `.app` → 回退手动拖拽」同一条纪律。
/// - **Linux**：只有 AppImage 形态能自删（`$APPIMAGE` 由 AppImage 运行时自己设，删文件不影响
///   已挂载的运行实例）。`/usr` 下的包管理器安装**故意不碰**：绕过 dpkg/rpm 删文件会留下
///   「包数据库说装着、磁盘上没有」的坏态，比不删更糟。
/// - **Windows**：运行中的 `.exe` 被文件系统锁住，**进程不能删自己**。唯一正路是拉起 NSIS
///   `uninstall.exe`（它会等本进程退出再删）。便携版没有 uninstaller ⇒ 只能手动删。
#[must_use]
pub fn plan_app_removal(
    os: &str,
    exe: &Path,
    appimage: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> AppRemoval {
    match os {
        "macos" => mac_app_bundle_from_exe(exe).map_or_else(
            || {
                AppRemoval::Unsupported(format!(
                    "当前可执行文件不在 .app 包内（{}）—— 多为开发构建或裸二进制，无法定位应用本体。\
                     不猜路径，请手动删除该文件",
                    exe.display()
                ))
            },
            AppRemoval::RemoveDir,
        ),
        "linux" => {
            if let Some(img) = appimage {
                return AppRemoval::RemoveFile(img.to_path_buf());
            }
            if exe.starts_with("/usr") {
                return AppRemoval::Unsupported(format!(
                    "检测到系统包管理器安装（{}）。绕过 dpkg/rpm 直接删文件会让包数据库与磁盘不一致，\
                     故本步骤不执行 —— 请用 apt/dnf 等包管理器卸载 polaris",
                    exe.display()
                ));
            }
            AppRemoval::Unsupported(format!(
                "无法判定 Linux 安装形态（既非 AppImage，也不在 /usr 下）：{}。\
                 不猜路径，请手动删除该目录",
                exe.display()
            ))
        }
        "windows" => {
            let Some(dir) = exe.parent() else {
                return AppRemoval::Unsupported(format!(
                    "定位不到安装目录（{}），请从「设置 › 应用」卸载 Polaris",
                    exe.display()
                ));
            };
            let uninstaller = dir.join("uninstall.exe");
            if exists(&uninstaller) {
                return AppRemoval::LaunchUninstaller(uninstaller);
            }
            AppRemoval::Unsupported(format!(
                "未找到 NSIS 卸载程序（{}）—— 多为便携版。Windows 上运行中的 .exe 被系统锁住，\
                 进程无法删除自己，请退出 Polaris 后手动删除 {}",
                uninstaller.display(),
                dir.display()
            ))
        }
        other => AppRemoval::Unsupported(format!("平台 {other} 无应用本体删除实现，请手动删除")),
    }
}

/// 执行应用本体删除计划。`spawn` 注入 ⇒ Windows 腿在单测里**不真起进程**也能断言。
///
/// 每条腿都先跑对应的白名单判定再动手（`RemoveDir`/`RemoveFile` 各自的 validate）。
#[must_use]
pub fn execute_app_removal(
    plan: AppRemoval,
    spawn: &dyn Fn(&Path) -> Result<(), String>,
) -> StepOutcome {
    match plan {
        AppRemoval::Unsupported(why) => StepOutcome::unsupported(why),
        AppRemoval::RemoveDir(dir) => match validate_app_bundle(&dir) {
            Err(PathReject::Missing) => {
                StepOutcome::skipped(format!("应用本体已不在原处（{}）", dir.display()))
            }
            Err(r) => StepOutcome::failed(format!("拒绝删除 {}：{}", dir.display(), r.reason())),
            Ok(()) => match std::fs::remove_dir_all(&dir) {
                Ok(()) => StepOutcome::done(format!("已删除应用本体 {}", dir.display())),
                Err(e) => StepOutcome::failed(format!("删除 {} 失败：{e}", dir.display())),
            },
        },
        AppRemoval::RemoveFile(file) => match validate_appimage(&file) {
            Err(PathReject::Missing) => {
                StepOutcome::skipped(format!("应用本体已不在原处（{}）", file.display()))
            }
            Err(r) => StepOutcome::failed(format!("拒绝删除 {}：{}", file.display(), r.reason())),
            Ok(()) => match std::fs::remove_file(&file) {
                Ok(()) => StepOutcome::done(format!("已删除 AppImage {}", file.display())),
                Err(e) => StepOutcome::failed(format!("删除 {} 失败：{e}", file.display())),
            },
        },
        // 措辞刻意是「已启动」而不是「已删除」：这一步交出去的是控制权，不是结果。
        AppRemoval::LaunchUninstaller(p) => match spawn(&p) {
            Ok(()) => StepOutcome::done(format!(
                "已启动 Windows 卸载程序 {} —— 应用本体需在它的窗口中完成卸载（本步骤不代表已删除）",
                p.display()
            )),
            Err(e) => StepOutcome::failed(format!("启动卸载程序 {} 失败：{e}", p.display())),
        },
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 纯编排：固定序 + fail-fast
// ────────────────────────────────────────────────────────────────────────────

/// 三条删除腿的注入面。真实实现见 [`SystemUninstallOps`]；单测注入替身，**一个真实路径都不碰**。
pub trait UninstallOps {
    /// 取消开机自启注册（OS 级登录项）。
    fn disable_autostart(&self) -> StepOutcome;
    /// 卸载提权 helper（其 root 脚本同时清受保护目录中的内核）。
    fn uninstall_helper(&self) -> StepOutcome;
    /// 删用户配置目录。
    fn remove_user_config(&self) -> StepOutcome;
    /// 删更新包缓存目录。
    fn remove_cache_dir(&self) -> StepOutcome;
    /// 清应用的 UserDefaults 域（macOS 才有内容）。
    fn remove_preferences(&self) -> StepOutcome;
    /// 删应用本体。
    fn remove_app(&self) -> StepOutcome;
}

/// **纯编排**：按 [`UninstallStep::DELETE_ORDER`] 依次执行，任一步 `Failed` 即停，
/// 其后各步一律记 `NotAttempted`（带上是谁把它拦下的）。
///
/// # 为什么必须 fail-fast，而不是「尽力删完」
///
/// 每一步失败都会让后一步变得**更危险**，而不只是「少删一样」：
/// - 停核失败还继续 ⇒ root 孤儿核 + 应用被删 = 用户断网且无处补救；
/// - 卸 helper 失败还继续删配置 ⇒ 配置里的 app 侧 token 没了，helper 从此**永远卸不掉**
///   （`HelperManager::uninstall` 要从配置目录读 token、往那里写提权脚本）；
/// - 删配置失败还继续删应用本体 ⇒ 应用没了，残留配置再没有任何 UI 能清。
///
/// 所以「上一步失败就别删下一项」不是保守，是唯一正确的传播方式。
#[must_use]
pub fn run_uninstall(ops: &dyn UninstallOps, stop_core: StepOutcome) -> UninstallReport {
    // 停核腿虽然不是删除动作，但它失败同样要拦下后面所有删除（理由见 `stop_core_outcome` 文档）。
    let mut halted = stop_core.is_failure().then_some(UninstallStep::StopCore);
    let mut steps = vec![StepReport {
        step: UninstallStep::StopCore,
        outcome: stop_core,
    }];

    // 执行序**读 [`UninstallStep::DELETE_ORDER`]**，而不是在这里另排一份。
    //
    // 早先这里是一个自带顺序的 `legs` 数组，`DELETE_ORDER` 只被单测引用 —— 于是那个常量成了
    // 一句没人执行的注释：把它改坏（比如对调 Helper / UserConfig），生产行为纹丝不动，
    // 顺序守卫照样绿（实测如此）。顺序是本模块最要紧的不变式，它必须只有**一个**声明处。
    let dispatch = |step: UninstallStep| -> StepOutcome {
        match step {
            UninstallStep::Autostart => ops.disable_autostart(),
            UninstallStep::Helper => ops.uninstall_helper(),
            UninstallStep::UserConfig => ops.remove_user_config(),
            UninstallStep::CacheDir => ops.remove_cache_dir(),
            UninstallStep::Preferences => ops.remove_preferences(),
            UninstallStep::AppBundle => ops.remove_app(),
            // 停核腿是前置条件，不该出现在删除序列里。真出现说明 `DELETE_ORDER` 被改坏了 ——
            // 记失败并触发 fail-fast，比 panic 掉整条命令好（用户至少拿得到一份如实报告）。
            UninstallStep::StopCore => {
                StepOutcome::failed("内部错误：停核腿不应出现在删除序列中，卸载已中止")
            }
        }
    };

    for step in UninstallStep::DELETE_ORDER {
        let outcome = match halted {
            Some(blocker) => StepOutcome::NotAttempted {
                detail: format!("未执行：「{}」失败后已中止卸载", blocker.label()),
            },
            None => dispatch(step),
        };
        if outcome.is_failure() {
            halted = Some(step);
        }
        steps.push(StepReport { step, outcome });
    }

    UninstallReport {
        verdict: verdict_of(&steps),
        requires_exit: requires_exit_of(&steps),
        steps,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 最外层薄壳：真实文件系统 + 提权通道
// ────────────────────────────────────────────────────────────────────────────

/// 「卸载提权 helper」这一个能力的窄 trait —— 与 [`HelperStopOps`](crate::runtime::helper::HelperStopOps)
/// 同一套路：单测既起不了真 daemon、也不许弹提权框，没有替身这条腿就完全无法断言。
pub trait HelperUninstallOps {
    /// 本平台是否有提权 helper 实现。
    fn supported(&self) -> bool;
    /// 是否已安装（未装则整步跳过，不该为此白弹一次提权框）。
    fn installed(&self) -> bool;
    /// 真卸载（弹一次提权框）。
    fn uninstall(&self) -> Result<(), String>;
    /// 本平台受保护目录中被一并清掉的内核路径（进报告，让用户知道到底删了哪儿）。
    fn protected_core_dir(&self) -> String;
}

/// 「取消开机自启」这一个能力的窄 trait。
///
/// 生产实现包一层 `tauri_plugin_autostart::AutoLaunchManager`（它要 `AppHandle`，单测构造不出）；
/// 抽出来后这条腿的三态（本就没开 / 摘掉了 / 摘不掉）才能被断言。
pub trait AutostartOps {
    /// 当前是否已注册开机自启。
    fn is_enabled(&self) -> bool;
    /// 注销登录项。
    fn disable(&self) -> Result<(), String>;
}

/// 生产实现：唯一碰真实 FS 与提权通道的地方。判定部分仍全部委给上面的纯函数。
pub struct SystemUninstallOps<'a, H: HelperUninstallOps, A: AutostartOps> {
    /// 提权 helper 面（生产是 `HelperRuntime`）。
    pub helper: &'a H,
    /// 开机自启面（生产是 `AutoLaunchManager` 的薄包装）。
    pub autostart: &'a A,
    /// 目标平台（`std::env::consts::OS`）。
    pub os: &'a str,
    /// 用户配置目录 = `<app_config_dir>/polaris`（由 `AppRuntime` 给，非前端入参）。
    pub config_dir: PathBuf,
    /// 更新包缓存目录 = `<app_cache_dir>/updates`（解析不到为 `None`）。
    pub cache_updates_dir: Option<PathBuf>,
    /// 应用 identifier = UserDefaults 域名（`tauri.conf.json` 的 `identifier`，非前端入参）。
    pub bundle_identifier: String,
    /// 当前可执行文件路径（`current_exe()`；取不到为 `None`）。
    pub exe: Option<PathBuf>,
    /// `$APPIMAGE`（仅 Linux AppImage 形态有值）。
    pub appimage: Option<PathBuf>,
}

/// 清掉应用的 UserDefaults 域（macOS）。
///
/// **走 API 不走文件**：`cfprefsd` 把域缓存在内存里，直接 `remove_file` 会被它按缓存写回来
/// （理由与出处见模块文档）。`removePersistentDomainForName:` 是 `defaults delete <domain>` 的代码
/// 等价形式，由 cfprefsd 自己落盘，故本函数**不返回失败**：它没有可失败的步骤，
/// 而编造一条永不触发的失败分支比没有分支更糟（同 [`crate::app_language::write_apple_languages`]）。
///
/// 清的是**整个域**而不只是 `AppleLanguages` 一个键：域里还会有 AppKit/WebKit 顺手写的窗口状态等键，
/// 它们同样是 Polaris 留下的痕迹；且只删一个键的话 plist 文件本身会留下来。
#[cfg(target_os = "macos")]
fn clear_preferences_domain(identifier: &str) -> StepOutcome {
    use objc2_foundation::{NSString, NSUserDefaults};

    let plist = std::env::var_os("HOME").map(|h| preferences_plist_path(Path::new(&h), identifier));
    // 存在性只用于**如实措辞**，不作为「要不要清」的判据：cfprefsd 的内存态可能尚未落盘，
    // 「文件不在」不等于「域是空的」，据此早退就会把内存里那份留到退出后被写出来。
    let existed = plist.as_deref().is_some_and(Path::exists);
    let at = plist.map_or_else(
        || "$HOME 未设，算不出 plist 路径".to_owned(),
        |p| p.display().to_string(),
    );

    NSUserDefaults::standardUserDefaults()
        .removePersistentDomainForName(&NSString::from_str(identifier));

    StepOutcome::done(if existed {
        format!(
            "已清除应用偏好域 {identifier}（{at}）—— 经 NSUserDefaults 交给 cfprefsd，未直接删 plist"
        )
    } else {
        format!(
            "已清除应用偏好域 {identifier}；清除前 {at} 并不存在（没改过应用内语言即如此）—— \
             仍发一次清除，防 cfprefsd 内存态在退出后被写出来"
        )
    })
}

/// 非 macOS 无此域：`AppleLanguages` 只在 macOS 写（[`crate::app_language`] 的两个入口在别的平台是空函数）。
///
/// 用 `Skipped` 而**不是** `Unsupported`：这里不是「本平台做不到」，是「本平台压根没有这东西」。
/// 判成 `Unsupported` 会让 Linux/Windows 上每一次干净卸载都被 [`verdict_of`] 降级成 `Incomplete`。
#[cfg(not(target_os = "macos"))]
fn clear_preferences_domain(identifier: &str) -> StepOutcome {
    StepOutcome::skipped(format!(
        "本平台没有 UserDefaults 域（{identifier}）：应用内语言只在 macOS 写进 AppleLanguages"
    ))
}

/// 分离式拉起 Windows 卸载程序：**不等它退出**（它要等本进程先退出才能删文件，等它就是死锁）。
fn spawn_uninstaller(path: &Path) -> Result<(), String> {
    std::process::Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

impl<H: HelperUninstallOps, A: AutostartOps> UninstallOps for SystemUninstallOps<'_, H, A> {
    fn disable_autostart(&self) -> StepOutcome {
        if !self.autostart.is_enabled() {
            return StepOutcome::skipped("未开启开机自启，无需注销");
        }
        match self.autostart.disable() {
            Ok(()) => StepOutcome::done("已注销开机自启登录项"),
            // 这条**必须**是硬失败：留着登录项 = 系统每次登录都去拉一个马上要被删掉的可执行文件。
            Err(e) => StepOutcome::failed(format!(
                "注销开机自启失败（{e}）：卸载已中止 —— 若继续删下去，系统每次登录都会尝试启动\
                 一个已不存在的 Polaris"
            )),
        }
    }

    fn remove_cache_dir(&self) -> StepOutcome {
        let Some(dir) = self.cache_updates_dir.as_deref() else {
            return StepOutcome::skipped("解析不到应用缓存目录，无更新包可清");
        };
        match validate_cache_updates_dir(dir) {
            Err(PathReject::Missing) => {
                StepOutcome::skipped(format!("更新包缓存目录不存在（{}）", dir.display()))
            }
            Err(r) => StepOutcome::failed(format!("拒绝删除 {}：{}", dir.display(), r.reason())),
            Ok(()) => match std::fs::remove_dir_all(dir) {
                Ok(()) => StepOutcome::done(format!("已删除更新包缓存 {}", dir.display())),
                Err(e) => StepOutcome::failed(format!("删除 {} 失败：{e}", dir.display())),
            },
        }
    }

    fn remove_preferences(&self) -> StepOutcome {
        let id = self.bundle_identifier.trim();
        // 先判定后清除（同其它删除腿）：域名一旦不是本应用那一个，最坏结果是抹掉用户全系统的偏好。
        if let Err(r) = validate_pref_domain(id) {
            return StepOutcome::failed(format!(
                "拒绝清除 UserDefaults 域「{id}」：{}",
                r.reason()
            ));
        }
        clear_preferences_domain(id)
    }

    fn uninstall_helper(&self) -> StepOutcome {
        if !self.helper.supported() {
            return StepOutcome::unsupported("当前平台没有提权助手实现");
        }
        if !self.helper.installed() {
            return StepOutcome::skipped(
                "提权助手未安装，无需卸载（受保护目录中也不会有受管内核）",
            );
        }
        match self.helper.uninstall() {
            Ok(()) => StepOutcome::done(format!(
                "已卸载提权助手，并一并清除受保护目录中的内核（{}）",
                self.helper.protected_core_dir()
            )),
            Err(e) => StepOutcome::failed(e),
        }
    }

    fn remove_user_config(&self) -> StepOutcome {
        let dir = &self.config_dir;
        match validate_config_dir(dir) {
            Err(PathReject::Missing) => {
                StepOutcome::skipped(format!("用户配置目录不存在（{}）", dir.display()))
            }
            Err(r) => StepOutcome::failed(format!("拒绝删除 {}：{}", dir.display(), r.reason())),
            Ok(()) => match std::fs::remove_dir_all(dir) {
                Ok(()) => StepOutcome::done(format!(
                    "已删除用户配置目录 {}（config.json / 订阅 / 规则 / 可写内核 core_update）",
                    dir.display()
                )),
                Err(e) => StepOutcome::failed(format!("删除 {} 失败：{e}", dir.display())),
            },
        }
    }

    fn remove_app(&self) -> StepOutcome {
        let Some(exe) = self.exe.as_deref() else {
            return StepOutcome::unsupported(
                "取不到当前可执行文件路径（current_exe 失败）—— 无法定位应用本体，请手动删除",
            );
        };
        let plan = plan_app_removal(self.os, exe, self.appimage.as_deref(), &|p| p.exists());
        execute_app_removal(plan, &spawn_uninstaller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ── 临时副本工具（**破坏性测试只碰这里造出来的副本，绝不碰真实安装**）────────────

    /// 独占临时目录；`Drop` 里清理（与本仓 `commands/updater.rs::scratch` 同款，无 tempfile dev-dep）。
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    impl Scratch {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let d = std::env::temp_dir().join(format!(
                "polaris-uninstall-test-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    /// 造一份**同构的**用户配置目录副本：`<scratch>/polaris/{config.json,core_update/sing-box,rules/}`。
    fn fake_config_dir(root: &Path) -> PathBuf {
        let dir = root.join(CONFIG_DIR_LEAF);
        std::fs::create_dir_all(dir.join("core_update")).unwrap();
        std::fs::create_dir_all(dir.join("rules")).unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        std::fs::write(dir.join("core_update").join("sing-box"), b"fake").unwrap();
        dir
    }

    /// 造一份同构的 macOS `.app` 包副本：`<scratch>/Polaris.app/Contents/MacOS/polaris`。
    fn fake_app_bundle(root: &Path) -> (PathBuf, PathBuf) {
        let bundle = root.join("Polaris.app");
        let macos = bundle.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("polaris");
        std::fs::write(&exe, b"fake").unwrap();
        (bundle, exe)
    }

    // ── 可注入替身 ──────────────────────────────────────────────────────────

    /// 记录调用序 + 可指定每条腿结果的 [`UninstallOps`] 替身。
    struct RecordingOps {
        calls: Mutex<Vec<UninstallStep>>,
        autostart: StepOutcome,
        helper: StepOutcome,
        config: StepOutcome,
        cache: StepOutcome,
        prefs: StepOutcome,
        app: StepOutcome,
    }
    impl RecordingOps {
        fn all_ok() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                autostart: StepOutcome::done("autostart"),
                helper: StepOutcome::done("helper"),
                config: StepOutcome::done("config"),
                cache: StepOutcome::done("cache"),
                prefs: StepOutcome::done("prefs"),
                app: StepOutcome::done("app"),
            }
        }
        fn calls(&self) -> Vec<UninstallStep> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl UninstallOps for RecordingOps {
        fn disable_autostart(&self) -> StepOutcome {
            self.calls.lock().unwrap().push(UninstallStep::Autostart);
            self.autostart.clone()
        }
        fn remove_cache_dir(&self) -> StepOutcome {
            self.calls.lock().unwrap().push(UninstallStep::CacheDir);
            self.cache.clone()
        }
        fn uninstall_helper(&self) -> StepOutcome {
            self.calls.lock().unwrap().push(UninstallStep::Helper);
            self.helper.clone()
        }
        fn remove_user_config(&self) -> StepOutcome {
            self.calls.lock().unwrap().push(UninstallStep::UserConfig);
            self.config.clone()
        }
        fn remove_preferences(&self) -> StepOutcome {
            self.calls.lock().unwrap().push(UninstallStep::Preferences);
            self.prefs.clone()
        }
        fn remove_app(&self) -> StepOutcome {
            self.calls.lock().unwrap().push(UninstallStep::AppBundle);
            self.app.clone()
        }
    }

    /// [`HelperUninstallOps`] 替身：不起 daemon、不弹提权框。
    struct FakeHelper {
        supported: bool,
        installed: bool,
        result: Result<(), String>,
        calls: AtomicUsize,
    }
    impl FakeHelper {
        fn ready() -> Self {
            Self {
                supported: true,
                installed: true,
                result: Ok(()),
                calls: AtomicUsize::new(0),
            }
        }
    }
    impl HelperUninstallOps for FakeHelper {
        fn supported(&self) -> bool {
            self.supported
        }
        fn installed(&self) -> bool {
            self.installed
        }
        fn uninstall(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
        fn protected_core_dir(&self) -> String {
            "/fake/protected/core".to_owned()
        }
    }

    /// [`AutostartOps`] 替身：不碰真实登录项（那是 launchd/注册表/.desktop，单测绝不该动）。
    struct FakeAutostart {
        enabled: bool,
        result: Result<(), String>,
        calls: AtomicUsize,
    }
    impl FakeAutostart {
        fn off() -> Self {
            Self {
                enabled: false,
                result: Ok(()),
                calls: AtomicUsize::new(0),
            }
        }
        fn on() -> Self {
            Self {
                enabled: true,
                ..Self::off()
            }
        }
    }
    impl AutostartOps for FakeAutostart {
        fn is_enabled(&self) -> bool {
            self.enabled
        }
        fn disable(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn outcome_of(r: &UninstallReport, step: UninstallStep) -> &StepOutcome {
        &r.steps.iter().find(|s| s.step == step).unwrap().outcome
    }

    // ── 编排：顺序 ──────────────────────────────────────────────────────────

    /// 🟡 **变异锁：删除腿的因果序不可变动。**
    ///
    /// 顺序错了不是「风格问题」：把 `UserConfig` 排到 `Helper` 前面，helper 的提权脚本就没地方落、
    /// app 侧 token 也没得读 ⇒ helper 永远卸不掉（见模块文档的表）。
    ///
    /// **变异探针**：把 [`UninstallStep::DELETE_ORDER`] 里任意两项对调 ⇒ 本条转红。
    #[test]
    fn delete_legs_run_in_causal_order() {
        let ops = RecordingOps::all_ok();
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert_eq!(
            ops.calls(),
            vec![
                UninstallStep::Autostart,
                UninstallStep::Helper,
                UninstallStep::UserConfig,
                UninstallStep::CacheDir,
                UninstallStep::Preferences,
                UninstallStep::AppBundle
            ],
            "先注销登录项（最便宜可逆）→ 卸 helper（它要用配置目录）→ 删配置 → 清更新缓存 \
             → 清应用偏好域（本进程还在跑，越晚清回写窗口越小）→ 最后删应用本体（它是当前进程的载体）"
        );
        assert_eq!(report.verdict, UninstallVerdict::Complete);
        assert_eq!(report.steps.len(), 7, "七个步骤必须逐项出现在报告里");
    }

    /// 报告里第一条恒为停核腿（它是前置条件，排在所有删除之前）。
    #[test]
    fn report_leads_with_the_stop_core_leg() {
        let ops = RecordingOps::all_ok();
        let report = run_uninstall(&ops, StepOutcome::skipped("no core"));
        assert_eq!(report.steps[0].step, UninstallStep::StopCore);
    }

    // ── 编排：失败传播（红线「上一步失败不得继续删下一项」）──────────────────

    /// 🟡 **变异锁：停核失败 ⇒ 一项都不许删。**
    ///
    /// **变异探针**：把 `run_uninstall` 里 `stop_core.is_failure().then_some(...)` 换成 `None`
    /// ⇒ 三条删除腿都会被调用 ⇒ 本条转红。
    #[test]
    fn stop_core_failure_blocks_every_delete() {
        let ops = RecordingOps::all_ok();
        let report = run_uninstall(&ops, StepOutcome::failed("core still alive"));
        assert!(
            ops.calls().is_empty(),
            "停核失败后一个删除动作都不许发生 —— 否则终局是 root 孤儿核 + 应用被删 = 断网且无处补救"
        );
        for step in UninstallStep::DELETE_ORDER {
            assert!(
                matches!(outcome_of(&report, step), StepOutcome::NotAttempted { .. }),
                "{step:?} 必须如实记为「未执行」，而不是悄悄消失"
            );
        }
        assert_eq!(report.verdict, UninstallVerdict::Failed);
    }

    /// 🟡 **变异锁：卸 helper 失败 ⇒ 不许继续删配置与应用本体。**
    ///
    /// **变异探针**：把 `if outcome.is_failure() { halted = Some(step); }` 删掉 ⇒ 本条转红。
    #[test]
    fn helper_failure_blocks_the_remaining_deletes() {
        let mut ops = RecordingOps::all_ok();
        ops.helper = StepOutcome::failed("用户取消了管理员授权");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert_eq!(
            ops.calls(),
            vec![UninstallStep::Autostart, UninstallStep::Helper],
            "helper 失败后不得再删配置（配置里的 token 一没，helper 就永远卸不掉了）"
        );
        let cfg = outcome_of(&report, UninstallStep::UserConfig);
        match cfg {
            StepOutcome::NotAttempted { detail } => {
                assert!(
                    detail.contains("卸载提权助手"),
                    "必须点名是谁把它拦下的：{detail}"
                );
            }
            other => panic!("配置腿应为 NotAttempted，实得 {other:?}"),
        }
        assert_eq!(report.verdict, UninstallVerdict::Failed);
    }

    /// 删配置失败 ⇒ 应用本体不许删（否则残留配置再没有任何 UI 能清）。
    #[test]
    fn config_failure_blocks_app_removal() {
        let mut ops = RecordingOps::all_ok();
        ops.config = StepOutcome::failed("permission denied");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert_eq!(
            ops.calls(),
            vec![
                UninstallStep::Autostart,
                UninstallStep::Helper,
                UninstallStep::UserConfig
            ]
        );
        assert!(matches!(
            outcome_of(&report, UninstallStep::AppBundle),
            StepOutcome::NotAttempted { .. }
        ));
    }

    // ── 编排：整体判定（红线「删了一半绝不能报成功」）────────────────────────

    /// 🟡 **变异锁：删了一半绝不能判成功。**
    ///
    /// **变异探针**：把 [`verdict_of`] 里 `Failed | NotAttempted` 那条早退删掉 ⇒ 本条转红。
    #[test]
    fn partial_deletion_is_never_complete() {
        let mut ops = RecordingOps::all_ok();
        ops.config = StepOutcome::failed("boom");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert_ne!(
            report.verdict,
            UninstallVerdict::Complete,
            "helper 已删、配置没删掉 —— 这是部分成功，判成 Complete 就是假成功"
        );
        assert_eq!(report.verdict, UninstallVerdict::Failed);
    }

    /// 🟡 **变异锁：有 `Unsupported` 就不是 Complete（Windows 便携版的常态）。**
    ///
    /// **变异探针**：把 `verdict_of` 里 `Unsupported` 那条早退删掉 ⇒ 本条转红。
    #[test]
    fn unsupported_step_downgrades_to_incomplete() {
        let mut ops = RecordingOps::all_ok();
        ops.app = StepOutcome::unsupported("Windows 便携版无 uninstaller");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert_eq!(
            report.verdict,
            UninstallVerdict::Incomplete,
            "应用本体还在原地 ⇒ 不是「完全卸载」，必须让用户知道还剩什么要手动做"
        );
    }

    /// `Skipped` **不**降级：helper 本就没装，是一次干净的完全卸载。
    ///
    /// **变异探针**：把 `verdict_of` 的 `Unsupported` 判据放宽到也包含 `Skipped` ⇒ 本条转红。
    #[test]
    fn skipped_step_still_counts_as_complete() {
        let mut ops = RecordingOps::all_ok();
        ops.helper = StepOutcome::skipped("未安装");
        let report = run_uninstall(&ops, StepOutcome::skipped("代理没跑"));
        assert_eq!(report.verdict, UninstallVerdict::Complete);
    }

    /// 删过配置或应用本体 ⇒ 必须提示退出；一步都没删成 ⇒ 不提示。
    #[test]
    fn requires_exit_tracks_real_deletions() {
        let ops = RecordingOps::all_ok();
        assert!(run_uninstall(&ops, StepOutcome::done("s")).requires_exit);

        let ops = RecordingOps::all_ok();
        assert!(
            !run_uninstall(&ops, StepOutcome::failed("s")).requires_exit,
            "一项都没删就不该催用户退出"
        );

        let mut ops = RecordingOps::all_ok();
        ops.config = StepOutcome::skipped("不存在");
        ops.app = StepOutcome::unsupported("做不到");
        assert!(!run_uninstall(&ops, StepOutcome::done("s")).requires_exit);
    }

    // ── 停核腿真值表 ────────────────────────────────────────────────────────

    /// 🟡 **变异锁：停核三态各自映射到不同结果。**
    ///
    /// **变异探针**：把 `stop_core_outcome` 的 `Some(e)` 腿改成 `StepOutcome::done(..)`
    /// （即恢复 helper 单卸载那条「停不掉也继续」的语义）⇒ 本条转红，且
    /// [`stop_core_failure_blocks_every_delete`] 一并转红。
    #[test]
    fn stop_core_outcome_truth_table() {
        assert!(matches!(
            stop_core_outcome(UninstallPreflight::ProceedDirectly, None),
            StepOutcome::Skipped { .. }
        ));
        assert!(
            matches!(
                stop_core_outcome(UninstallPreflight::ProceedDirectly, Some("ignored")),
                StepOutcome::Skipped { .. }
            ),
            "没发起过停核 ⇒ 不该因为一个陈旧的错误串就报失败"
        );
        assert!(matches!(
            stop_core_outcome(UninstallPreflight::StopCoreFirst, None),
            StepOutcome::Done { .. }
        ));
        let failed = stop_core_outcome(UninstallPreflight::StopCoreFirst, Some("EPERM"));
        assert!(failed.is_failure(), "完全卸载里停不掉核必须是硬失败");
        match failed {
            StepOutcome::Failed { detail } => assert!(detail.contains("EPERM"), "原因必须原样带出"),
            other => panic!("实得 {other:?}"),
        }
    }

    // ── 应用本体：三平台可行性 ──────────────────────────────────────────────

    /// 🟡 **变异锁：三平台各自的可行/不可行判定。**
    ///
    /// 这组是本任务里**唯一**能覆盖 mac/win 腿的手段（开发机是 Linux，真机又不许跑卸载）。
    /// **变异探针**：把 macOS 腿的 `mac_app_bundle_from_exe` 换成恒 `None` ⇒ 第 1 条转红；
    /// 把 Windows 腿的 `exists(&uninstaller)` 换成恒 true ⇒ 第 5 条转红；
    /// 把 Linux 的 `/usr` 判据删掉 ⇒ 第 4 条转红。
    #[test]
    fn plan_app_removal_covers_all_three_platforms() {
        let never = |_: &Path| false;
        let always = |_: &Path| true;

        // 1. macOS 正常安装 → 删 .app 包。
        assert_eq!(
            plan_app_removal(
                "macos",
                Path::new("/Applications/Polaris.app/Contents/MacOS/polaris"),
                None,
                &never
            ),
            AppRemoval::RemoveDir(PathBuf::from("/Applications/Polaris.app"))
        );
        // 2. macOS 开发构建（不在 .app 内）→ 不猜路径。
        assert!(matches!(
            plan_app_removal(
                "macos",
                Path::new("/home/dev/target/debug/polaris"),
                None,
                &never
            ),
            AppRemoval::Unsupported(_)
        ));
        // 3. Linux AppImage → 删那个文件。
        assert_eq!(
            plan_app_removal(
                "linux",
                Path::new("/tmp/.mount_abc/AppRun"),
                Some(Path::new("/home/u/Apps/Polaris-0.1.0.AppImage")),
                &never
            ),
            AppRemoval::RemoveFile(PathBuf::from("/home/u/Apps/Polaris-0.1.0.AppImage"))
        );
        // 4. Linux 包管理器安装 → **故意不碰**。
        match plan_app_removal("linux", Path::new("/usr/bin/polaris"), None, &never) {
            AppRemoval::Unsupported(why) => {
                assert!(why.contains("包管理器"), "必须说清为什么不删：{why}");
            }
            other => panic!("绕过 dpkg/rpm 删文件会留下坏态，必须 Unsupported，实得 {other:?}"),
        }
        // 5. Windows 有 NSIS uninstaller → 拉起它（进程删不掉自己的 .exe）。
        //
        // ⚠️ 字面量用**正斜杠**：`Path` 的分隔符是**宿主**的，在 Linux 上跑测时 `C:\a\b` 会被当成
        // 单个文件名、`parent()` 返空 —— 那样测到的是 Linux 的解析规则，不是本函数的判定。
        // 正斜杠在 Windows 上同样合法，两边都能正确切出 parent，故它才是这条断言的正确取材。
        // （同一个坑 `InstallPaths::win()` 已踩过并成文：`PathBuf::join` 用宿主分隔符。）
        assert_eq!(
            plan_app_removal(
                "windows",
                Path::new("C:/Program Files/Polaris/polaris.exe"),
                None,
                &always
            ),
            AppRemoval::LaunchUninstaller(
                Path::new("C:/Program Files/Polaris").join("uninstall.exe")
            )
        );
        // 6. Windows 便携版（无 uninstaller）→ 如实说做不到 + 给手动路径。
        match plan_app_removal(
            "windows",
            Path::new("D:/portable/polaris.exe"),
            None,
            &never,
        ) {
            AppRemoval::Unsupported(why) => assert!(why.contains("手动删除"), "{why}"),
            other => panic!("实得 {other:?}"),
        }
        // 7. 未知平台。
        assert!(matches!(
            plan_app_removal("freebsd", Path::new("/usr/local/bin/polaris"), None, &never),
            AppRemoval::Unsupported(_)
        ));
    }

    /// Windows 腿**不真起进程**也能断言：`spawn` 注入 + 措辞必须是「已启动」而非「已删除」。
    #[test]
    fn windows_leg_reports_launch_not_deletion() {
        let seen = Mutex::new(Vec::<PathBuf>::new());
        let spawn = |p: &Path| {
            seen.lock().unwrap().push(p.to_path_buf());
            Ok(())
        };
        let out = execute_app_removal(
            AppRemoval::LaunchUninstaller(PathBuf::from(r"C:\Program Files\Polaris\uninstall.exe")),
            &spawn,
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "必须真去拉 uninstaller");
        match out {
            StepOutcome::Done { detail } => {
                assert!(detail.contains("已启动"), "{detail}");
                assert!(
                    detail.contains("不代表已删除"),
                    "拉起 uninstaller ≠ 应用本体已删 —— 措辞不能骗人：{detail}"
                );
            }
            other => panic!("实得 {other:?}"),
        }
        // 拉不起来必须是失败，不能静默当成功。
        let fail = execute_app_removal(
            AppRemoval::LaunchUninstaller(PathBuf::from(r"C:\x\uninstall.exe")),
            &|_| Err("ACCESS_DENIED".to_owned()),
        );
        assert!(fail.is_failure());
    }

    // ── 路径判定：白名单（破坏性腿只碰临时副本）────────────────────────────

    #[test]
    fn reject_relative_path() {
        assert_eq!(
            validate_config_dir(Path::new("polaris")),
            Err(PathReject::NotAbsolute)
        );
    }

    #[test]
    fn reject_leaf_mismatch() {
        let s = Scratch::new("leaf");
        let other = s.path().join("not-polaris");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(validate_config_dir(&other), Err(PathReject::LeafMismatch));
        assert!(other.exists(), "被拒的路径必须原封不动");
    }

    #[test]
    fn reject_too_shallow() {
        // `/polaris`：叶名对得上，但没有具名父目录 ⇒ 必须拒。
        let shallow = if cfg!(windows) {
            PathBuf::from(r"C:\polaris")
        } else {
            PathBuf::from("/polaris")
        };
        assert_eq!(validate_config_dir(&shallow), Err(PathReject::TooShallow));
    }

    #[test]
    fn reject_missing() {
        let s = Scratch::new("missing");
        assert_eq!(
            validate_config_dir(&s.path().join(CONFIG_DIR_LEAF)),
            Err(PathReject::Missing)
        );
    }

    /// 🟡 **变异锁：软链必须被拒（跟随删除会删到链外的任意位置）。**
    ///
    /// **变异探针**：把 `validate_removable` 的 `symlink_metadata` 换成 `metadata` ⇒ 本条转红。
    #[cfg(unix)]
    #[test]
    fn reject_symlinked_dir() {
        let s = Scratch::new("symlink");
        let real = s.path().join("real-target");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("precious.txt"), b"must survive").unwrap();
        let link = s.path().join(CONFIG_DIR_LEAF);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(validate_config_dir(&link), Err(PathReject::Symlink));
        assert!(real.join("precious.txt").exists(), "链外内容必须毫发无伤");
    }

    #[test]
    fn reject_file_where_dir_expected() {
        let s = Scratch::new("kind");
        let f = s.path().join(CONFIG_DIR_LEAF);
        std::fs::write(&f, b"not a dir").unwrap();
        assert_eq!(validate_config_dir(&f), Err(PathReject::KindMismatch));
    }

    #[test]
    fn accept_a_well_formed_config_dir() {
        let s = Scratch::new("accept");
        let dir = fake_config_dir(s.path());
        assert_eq!(validate_config_dir(&dir), Ok(()));
    }

    // ── 薄壳：对着**临时副本**真删一次 ──────────────────────────────────────

    /// 配置腿在副本上跑通：目录连同可写内核一起消失，报告点名删了哪儿。
    #[test]
    fn config_leg_deletes_the_copy_and_reports_the_path() {
        let s = Scratch::new("cfgdel");
        let dir = fake_config_dir(s.path());
        let helper = FakeHelper::ready();
        let ops = SystemUninstallOps {
            helper: &helper,
            autostart: &FakeAutostart::off(),
            os: "linux",
            config_dir: dir.clone(),
            cache_updates_dir: None,
            bundle_identifier: "com.polaris.app".to_owned(),
            exe: None,
            appimage: None,
        };
        match ops.remove_user_config() {
            StepOutcome::Done { detail } => {
                assert!(detail.contains(&dir.display().to_string()), "{detail}");
            }
            other => panic!("实得 {other:?}"),
        }
        assert!(!dir.exists(), "副本应已删除");
    }

    /// 配置目录不存在 ⇒ `Skipped`，**不是** `Failed`（一次幂等的重试不该报错）。
    #[test]
    fn config_leg_skips_when_absent() {
        let s = Scratch::new("cfgabsent");
        let helper = FakeHelper::ready();
        let ops = SystemUninstallOps {
            helper: &helper,
            autostart: &FakeAutostart::off(),
            os: "linux",
            config_dir: s.path().join(CONFIG_DIR_LEAF),
            cache_updates_dir: None,
            bundle_identifier: "com.polaris.app".to_owned(),
            exe: None,
            appimage: None,
        };
        assert!(matches!(
            ops.remove_user_config(),
            StepOutcome::Skipped { .. }
        ));
    }

    /// 🟡 **变异锁：白名单不匹配 ⇒ 拒删且目录必须还在。**
    ///
    /// **变异探针**：把 `remove_user_config` 里的 `validate_config_dir` 调用删掉（直接 `remove_dir_all`）
    /// ⇒ 本条转红（目录会被真删）。
    #[test]
    fn config_leg_refuses_a_path_outside_the_whitelist() {
        let s = Scratch::new("cfgguard");
        let rogue = s.path().join("Documents");
        std::fs::create_dir_all(&rogue).unwrap();
        std::fs::write(rogue.join("thesis.txt"), b"10 years of work").unwrap();
        let helper = FakeHelper::ready();
        let ops = SystemUninstallOps {
            helper: &helper,
            autostart: &FakeAutostart::off(),
            os: "linux",
            config_dir: rogue.clone(),
            cache_updates_dir: None,
            bundle_identifier: "com.polaris.app".to_owned(),
            exe: None,
            appimage: None,
        };
        assert!(ops.remove_user_config().is_failure());
        assert!(
            rogue.join("thesis.txt").exists(),
            "白名单外的路径一个字节都不许动"
        );
    }

    /// macOS 腿在**副本 .app 包**上跑通（本机是 Linux，但这条腿只用到 `.app` 的路径形态与 FS 语义）。
    ///
    /// # 为什么排除 Windows（2026-08-05，Windows CI 腿首次跑通后实测）
    ///
    /// 判定入口 `update_install::mac_app_bundle_from_exe` 是**按 `/` 硬匹配** `".app/Contents/MacOS/"`
    /// 的字符串查找。那在它的实际作用域内正确 —— 它只被 `plan_app_removal` 的 `"macos"` 分支调用，
    /// 而 macOS 的路径分隔符恒为 `/`。**不是生产缺陷，Windows 上永远走不到这条腿。**
    ///
    /// 但本用例用 `Path::join` 造副本路径，在 Windows 上产出 `\` 分隔符（`…\Polaris.app\Contents\
    /// MacOS\polaris`）⇒ 查找落空 ⇒ 判 `Unsupported`。即「借 FS 语义」这个前提在 Windows 上不成立：
    /// 那里的路径语义与 macOS 不同，借不成。
    ///
    /// 用 `not(windows)` 而非 `target_os = "macos"`：Linux 才是它当前的主要运行环境（`/` 分隔符
    /// 使前提成立），门控成 macOS-only 等于把这条覆盖整个丢掉。
    #[cfg(not(windows))]
    #[test]
    fn mac_leg_deletes_a_copied_app_bundle() {
        let s = Scratch::new("appdel");
        let (bundle, exe) = fake_app_bundle(s.path());
        let helper = FakeHelper::ready();
        let ops = SystemUninstallOps {
            helper: &helper,
            autostart: &FakeAutostart::off(),
            os: "macos",
            config_dir: s.path().join(CONFIG_DIR_LEAF),
            cache_updates_dir: None,
            bundle_identifier: "com.polaris.app".to_owned(),
            exe: Some(exe),
            appimage: None,
        };
        match ops.remove_app() {
            StepOutcome::Done { detail } => assert!(detail.contains("Polaris.app"), "{detail}"),
            other => panic!("实得 {other:?}"),
        }
        assert!(!bundle.exists(), "副本 .app 应已删除");
    }

    /// Linux AppImage 腿在副本文件上跑通；叶名不像 AppImage 的一律拒。
    #[test]
    fn appimage_leg_deletes_a_copied_file_but_guards_the_leaf() {
        let s = Scratch::new("appimg");
        let img = s.path().join("Polaris-0.1.0.AppImage");
        std::fs::write(&img, b"fake").unwrap();
        assert!(matches!(
            execute_app_removal(AppRemoval::RemoveFile(img.clone()), &|_| Ok(())),
            StepOutcome::Done { .. }
        ));
        assert!(!img.exists());

        let decoy = s.path().join("important.tar.gz");
        std::fs::write(&decoy, b"payload").unwrap();
        assert!(
            execute_app_removal(AppRemoval::RemoveFile(decoy.clone()), &|_| Ok(())).is_failure()
        );
        assert!(decoy.exists(), "非 AppImage 叶名一个字节都不许动");
    }

    // ── 薄壳：helper 腿的三态 ───────────────────────────────────────────────

    #[test]
    fn helper_leg_three_states() {
        let s = Scratch::new("helperleg");
        let mk = |h: &FakeHelper| -> StepOutcome {
            SystemUninstallOps {
                helper: h,
                autostart: &FakeAutostart::off(),
                os: "linux",
                config_dir: s.path().join(CONFIG_DIR_LEAF),
                cache_updates_dir: None,
                bundle_identifier: "com.polaris.app".to_owned(),
                exe: None,
                appimage: None,
            }
            .uninstall_helper()
        };

        // 未安装 → 跳过，且**不该白弹一次提权框**。
        let not_installed = FakeHelper {
            installed: false,
            ..FakeHelper::ready()
        };
        assert!(matches!(mk(&not_installed), StepOutcome::Skipped { .. }));
        assert_eq!(
            not_installed.calls.load(Ordering::SeqCst),
            0,
            "没装还去调 uninstall = 平白弹一次要密码的框"
        );

        // 平台不支持 → 如实标 Unsupported。
        let unsupported = FakeHelper {
            supported: false,
            ..FakeHelper::ready()
        };
        assert!(matches!(mk(&unsupported), StepOutcome::Unsupported { .. }));

        // 用户取消提权 → 失败，原因原样带出。
        let cancelled = FakeHelper {
            result: Err("已取消管理员授权".to_owned()),
            ..FakeHelper::ready()
        };
        match mk(&cancelled) {
            StepOutcome::Failed { detail } => assert!(detail.contains("已取消管理员授权")),
            other => panic!("实得 {other:?}"),
        }

        // 成功 → 报告必须点名受保护目录（用户得知道到底删了哪儿）。
        let ok = FakeHelper::ready();
        match mk(&ok) {
            StepOutcome::Done { detail } => {
                assert!(detail.contains("/fake/protected/core"), "{detail}");
            }
            other => panic!("实得 {other:?}"),
        }
    }

    // ── 开机自启腿（OS 登录项在配置目录之外，漏掉就是「卸载不干净」的最痛一项）──────

    /// 🟡 **变异锁：登录项三态 + 摘不掉必须硬失败。**
    ///
    /// 这条腿删的东西**不在配置目录里**（macOS LaunchAgent plist / Windows 注册表 Run 键 /
    /// Linux `~/.config/autostart/*.desktop`），删配置目录顺手带不走它。留着的后果是永久性的：
    /// 应用都没了，系统每次登录还去拉那个不存在的可执行文件。
    ///
    /// **变异探针**：把 `disable_autostart` 的 `Err` 腿改成 `StepOutcome::skipped(..)`
    /// ⇒ 第 3 段转红；把 `is_enabled()` 判定删掉（无条件 disable）⇒ 第 1 段的调用次数断言转红。
    #[test]
    fn autostart_leg_three_states() {
        let s = Scratch::new("autostart");
        let helper = FakeHelper::ready();
        let mk = |a: &FakeAutostart| -> StepOutcome {
            SystemUninstallOps {
                helper: &helper,
                autostart: a,
                os: "linux",
                config_dir: s.path().join(CONFIG_DIR_LEAF),
                cache_updates_dir: None,
                bundle_identifier: "com.polaris.app".to_owned(),
                exe: None,
                appimage: None,
            }
            .disable_autostart()
        };

        // 1. 本就没开 → 跳过，且**不去动登录项**（没开还去 disable 是无谓的系统写操作）。
        let off = FakeAutostart::off();
        assert!(matches!(mk(&off), StepOutcome::Skipped { .. }));
        assert_eq!(off.calls.load(Ordering::SeqCst), 0);

        // 2. 开着 → 注销成功。
        let on = FakeAutostart::on();
        assert!(matches!(mk(&on), StepOutcome::Done { .. }));
        assert_eq!(on.calls.load(Ordering::SeqCst), 1);

        // 3. 摘不掉 → **硬失败**（fail-fast 会据此拦下后面所有删除）。
        let stuck = FakeAutostart {
            result: Err("registry access denied".to_owned()),
            ..FakeAutostart::on()
        };
        let out = mk(&stuck);
        assert!(
            out.is_failure(),
            "登录项摘不掉却继续删，等于给用户留一个永久报错的登录项"
        );
        match out {
            StepOutcome::Failed { detail } => assert!(detail.contains("registry access denied")),
            other => panic!("实得 {other:?}"),
        }
    }

    /// 🟡 **变异锁：登录项摘不掉 ⇒ helper / 配置 / 应用本体一项都不许删。**
    ///
    /// **变异探针**：把 `DELETE_ORDER` 里的 `Autostart` 挪到末尾 ⇒ 本条转红（前面几项已被删）。
    #[test]
    fn autostart_failure_blocks_everything_after_it() {
        let mut ops = RecordingOps::all_ok();
        ops.autostart = StepOutcome::failed("摘不掉");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert_eq!(
            ops.calls(),
            vec![UninstallStep::Autostart],
            "登录项这一步失败后，一个删除动作都不该发生（此时代价还是零）"
        );
        for step in [
            UninstallStep::Helper,
            UninstallStep::UserConfig,
            UninstallStep::CacheDir,
            UninstallStep::AppBundle,
        ] {
            assert!(matches!(
                outcome_of(&report, step),
                StepOutcome::NotAttempted { .. }
            ));
        }
    }

    // ── 更新包缓存腿（`app_cache_dir()/updates`，同样在配置目录之外）────────────

    /// 缓存腿在**副本**上跑通；叶名不是 `updates` 的一律拒（白名单同款）。
    ///
    /// **变异探针**：把 `remove_cache_dir` 里的 `validate_cache_updates_dir` 去掉 ⇒ 第 2 段转红。
    #[test]
    fn cache_leg_deletes_the_copy_and_guards_the_leaf() {
        let s = Scratch::new("cache");
        let helper = FakeHelper::ready();
        let autostart = FakeAutostart::off();
        let mk = |dir: Option<PathBuf>| -> StepOutcome {
            SystemUninstallOps {
                helper: &helper,
                autostart: &autostart,
                os: "linux",
                config_dir: s.path().join(CONFIG_DIR_LEAF),
                cache_updates_dir: dir,
                bundle_identifier: "com.polaris.app".to_owned(),
                exe: None,
                appimage: None,
            }
            .remove_cache_dir()
        };

        // 1. 正常：副本被删。
        let updates = s.path().join(CACHE_UPDATES_LEAF);
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("Polaris-0.1.1.AppImage"), b"installer").unwrap();
        assert!(matches!(
            mk(Some(updates.clone())),
            StepOutcome::Done { .. }
        ));
        assert!(!updates.exists());

        // 2. 叶名不在白名单 → 拒删且目录必须还在。
        let rogue = s.path().join("Downloads");
        std::fs::create_dir_all(&rogue).unwrap();
        std::fs::write(rogue.join("keep.bin"), b"x").unwrap();
        assert!(mk(Some(rogue.clone())).is_failure());
        assert!(rogue.join("keep.bin").exists(), "白名单外一个字节都不许动");

        // 3. 不存在 / 解析不到 → Skipped（幂等重试不该报错）。
        assert!(matches!(mk(Some(updates)), StepOutcome::Skipped { .. }));
        assert!(matches!(mk(None), StepOutcome::Skipped { .. }));
    }

    // ── Preferences 域腿（macOS `~/Library/Preferences/<id>.plist`，同样在配置目录之外）────
    //
    // 真清除只在 macOS 上发生且**不可测**（本机是 Linux；真跑一次会清掉真实用户的偏好域）。
    // 故这一族测的是：清单里有没有它、序在哪、域名判定挡不挡得住误清、路径拼得对不对。

    /// 🟡 **变异锁：Preferences 域必须在删除清单里，且排在应用本体之前。**
    ///
    /// 漏掉它 = 卸载完 `~/Library/Preferences/com.polaris.app.plist` 还躺着一条
    /// 「这台机器的 Polaris 被设成过俄语」的记录，重装后以一个用户没设过的语言启动。
    /// 排到 `AppBundle` 之后 = 那一步之后已经没有代码可执行（mac 上 `.app` 已被删）。
    ///
    /// **变异探针**：把 `Preferences` 从 [`UninstallStep::DELETE_ORDER`] 里删掉 ⇒ 本条转红；
    /// 与它对调 `AppBundle` ⇒ 第 2 段转红。
    #[test]
    fn preferences_leg_is_in_the_delete_list_before_the_app_bundle() {
        let order = UninstallStep::DELETE_ORDER;
        let at = order.iter().position(|s| *s == UninstallStep::Preferences);
        let app = order.iter().position(|s| *s == UninstallStep::AppBundle);
        assert!(
            at.is_some(),
            "Preferences 域不在删除清单里 —— 卸载会留下 ~/Library/Preferences/<id>.plist"
        );
        assert!(at < app, "必须早于删应用本体：那一步之后没有代码可执行了");

        // 编排层真的会调它（只在常量里写一笔而 `dispatch` 漏了 ⇒ 永远不执行）。
        let ops = RecordingOps::all_ok();
        let _ = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert!(
            ops.calls().contains(&UninstallStep::Preferences),
            "DELETE_ORDER 里有、dispatch 里没有 = 一条永不执行的清单项"
        );
    }

    /// 清偏好域失败 ⇒ 应用本体不许删（fail-fast 对新腿同样成立）。
    #[test]
    fn preferences_failure_blocks_app_removal() {
        let mut ops = RecordingOps::all_ok();
        ops.prefs = StepOutcome::failed("拒绝清除");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        assert!(!ops.calls().contains(&UninstallStep::AppBundle));
        assert!(matches!(
            outcome_of(&report, UninstallStep::AppBundle),
            StepOutcome::NotAttempted { .. }
        ));
    }

    /// 🟡 **变异锁：域名白名单挡得住「清掉用户全系统偏好」。**
    ///
    /// `removePersistentDomainForName:` 收到 `NSGlobalDomain` 会抹掉
    /// `~/Library/Preferences/.GlobalPreferences.plist` —— 用户的系统语言/区域/键盘设置，
    /// 与 Polaris 毫无关系。这是本判定存在的首要理由。
    ///
    /// **变异探针**：删掉 [`validate_pref_domain`] 里的 `GLOBAL_PREF_DOMAINS` 判据 ⇒ 本条转红。
    #[test]
    fn reject_global_pref_domains() {
        for d in [
            "NSGlobalDomain",
            "nsglobaldomain",
            ".GlobalPreferences",
            "kCFPreferencesAnyApplication",
        ] {
            assert_eq!(
                validate_pref_domain(d),
                Err(PrefDomainReject::Global),
                "{d} 必须被拒 —— 清它等于抹掉用户全系统的偏好"
            );
        }
    }

    /// 空 / 非反向 DNS 形态一律拒（identifier 已经不是本应用那一个了）。
    ///
    /// **变异探针**：删掉 `contains('.')` 判据 ⇒ `"polaris"` 那条转红。
    #[test]
    fn reject_malformed_pref_domain() {
        assert_eq!(validate_pref_domain(""), Err(PrefDomainReject::Empty));
        assert_eq!(validate_pref_domain("   "), Err(PrefDomainReject::Empty));
        for d in [
            "polaris",                      // 裸名：不是本应用的域
            ".hidden",                      // 点开头：`.GlobalPreferences` 那一族的形状
            "/Users/x/Library/Preferences", // 拿到的是路径不是域名
            "com.polaris.app/../other",     // 路径穿越
            "com.polaris app",              // 含空白
        ] {
            assert_eq!(
                validate_pref_domain(d),
                Err(PrefDomainReject::Malformed),
                "{d} 必须被拒"
            );
        }
        assert_eq!(validate_pref_domain("com.polaris.app"), Ok(()));
        assert_eq!(
            validate_pref_domain(" com.polaris.app "),
            Ok(()),
            "两侧空白应被 trim"
        );
    }

    /// 非 macOS 上这一步是 `Skipped` 而**不是** `Unsupported` ——
    /// 后者会让 Linux/Windows 上每一次干净卸载都被判成 `Incomplete`。
    ///
    /// **变异探针**：把非 macOS 那支的 `skipped` 改成 `unsupported` ⇒ 本条在 Linux/Windows 上转红。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn preferences_leg_is_skipped_not_unsupported_off_macos() {
        let s = Scratch::new("prefs");
        let helper = FakeHelper::ready();
        let autostart = FakeAutostart::off();
        let ops = SystemUninstallOps {
            helper: &helper,
            autostart: &autostart,
            os: "linux",
            config_dir: s.path().join(CONFIG_DIR_LEAF),
            cache_updates_dir: None,
            bundle_identifier: "com.polaris.app".to_owned(),
            exe: None,
            appimage: None,
        };
        assert!(matches!(
            ops.remove_preferences(),
            StepOutcome::Skipped { .. }
        ));

        // 域名坏掉时**照样**是硬失败（这条判定不分平台）。
        let bad = SystemUninstallOps {
            bundle_identifier: "NSGlobalDomain".to_owned(),
            ..ops
        };
        assert!(bad.remove_preferences().is_failure());
    }

    /// plist 路径拼装：`$HOME/Library/Preferences/<identifier>.plist`。
    ///
    /// 只进报告文案，但拼错的形态是「报告里指了个不存在的文件」，用户照着去看会以为没清干净。
    /// 与 `app_language::user_config_path` 那条同款：写死整条绝对路径，不只查叶名。
    #[test]
    fn preferences_plist_path_is_home_then_library_preferences_then_identifier() {
        assert_eq!(
            preferences_plist_path(Path::new("/Users/x"), "com.polaris.app"),
            Path::new("/Users/x/Library/Preferences/com.polaris.app.plist"),
        );
    }

    // ── 前端契约面 ──────────────────────────────────────────────────────────

    /// 报告的序列化形是前端逐项渲染的契约：字段名/步骤名/结果 kind 一变前端就哑了。
    #[test]
    fn report_serializes_the_frontend_contract() {
        let mut ops = RecordingOps::all_ok();
        ops.app = StepOutcome::unsupported("便携版");
        let report = run_uninstall(&ops, StepOutcome::done("stopped"));
        let v = serde_json::to_value(&report).unwrap();

        assert_eq!(v["verdict"], "incomplete");
        assert_eq!(v["requiresExit"], true);
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 7);
        assert_eq!(steps[0]["step"], "stopCore");
        assert_eq!(steps[1]["step"], "autostart");
        assert_eq!(steps[2]["step"], "helper");
        assert_eq!(steps[3]["step"], "userConfig");
        assert_eq!(steps[4]["step"], "cacheDir");
        assert_eq!(steps[5]["step"], "preferences");
        assert_eq!(steps[6]["step"], "appBundle");
        assert_eq!(steps[6]["outcome"]["kind"], "unsupported");
        assert_eq!(steps[6]["outcome"]["detail"], "便携版");
    }
}
