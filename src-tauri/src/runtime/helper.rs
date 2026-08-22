//! helper 运行时：`polaris-helper-client` + 平台 helper crate 装配。
//!
//! Polaris 锚点：`main/services/HelperManager.ts`（macOS LaunchDaemon / Windows SCM / Linux systemd
//! 提权 helper 的客户端：install / uninstall / getStatus / start / stop 核经 helper 提权）。
//!
//! 装配（C6-5 收口）：
//! - [`polaris_helper_client::HelperManager`]：helper 生命周期（install/uninstall + token + 状态探测）。
//! - [`polaris_helper_client::HelperClient`]：helper 通信（生产 `UnixConnector`（mac/linux）/
//!   `PipeConnector`（win），经 [`Connector`](polaris_helper_client::Connector) trait）。
//!
//! **真机门**：`install`/`uninstall` 触发一次提权（osascript / UAC / pkexec）——本机（Linux 桌面）
//! 无 bundled helper 二进制 → `resolve_helper_binary` 失败 → 早返「二进制缺失」，**绝不触发提权**（安全）。
//! `start_core`/`stop_core` 经就绪 daemon 的 socket/pipe 起停 root/SYSTEM 受管核——app→helper→核端到端
//! 提权起核是本计划头号真机门（部署首验）。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use polaris_helper_client::Connector;
#[cfg(windows)]
use polaris_helper_client::PipeConnector;
#[cfg(unix)]
use polaris_helper_client::UnixConnector;
use polaris_helper_client::{
    read_token, EscalationOutcome, HelperClient, HelperManager, HelperStatus, InstallParams,
    InstallPaths, StdExecutor, StdSysOps, SysOps, INSTALL_CORE_TIMEOUT_MS, READY_POLL_ATTEMPTS,
    READY_POLL_DELAY,
};
use polaris_helper_proto::{
    FlushDns, InstallCoreParams, LinuxStartParams, Platform, Request, Response, ResponseKind,
    RouteParams, Start, StartParams, Stop,
};
use polaris_system_integration::dns_flush::HelperFlushResult;
use std::time::Duration;

/// app 侧 token 文件名（= 上游 `getUserDataPath()/helper-client.token`，`HelperManager.ts:478`）。
const HELPER_TOKEN_FILE: &str = "helper-client.token";

/// helper 起核通信超时：daemon 在 spawn 子核后立即回 `OK started <pid>`（不等就绪，就绪门在 proxy 侧），
/// 故留 15s 余量覆盖偶发慢盘/大 config 校验足矣。
const HELPER_START_TIMEOUT: Duration = Duration::from_secs(15);

/// helper flush-dns 通信超时（对齐 上游 `HelperManager.flushDns` 的 5000ms；flush 为瞬时操作）。
const HELPER_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// helper route-add/route-del 通信超时（route 手术为瞬时操作，取 5s 同 flush 量级）。
const HELPER_ROUTE_TIMEOUT: Duration = Duration::from_secs(5);

/// helper 状态快照（上游 `HelperStatus` 镜像，序列化形与前端 `contracts/types/runtime.ts` 一致）。
///
/// 字段与前端 `HelperStatus` 逐字对齐（`SettingsHelper.tsx` 的 `deriveState` 消费 supported / upgradeable /
/// backgroundDisabled / needsRepair / ready / installed / version）。`loaded` / `backgroundDisabled` /
/// `pathMismatch` / `installedSingboxPath` 是 macOS 真机门（BTM/plist 烧录路径对比），本机不可判 → 惰值。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperStatusSnapshot {
    /// 当前平台是否支持提权 helper（mac/win/linux 三平台均有实现）。
    pub supported: bool,
    /// helper 二进制 + 描述符（plist/unit/SCM）是否在位。
    pub installed: bool,
    /// socket ping 成功且 proto ≥ 最低可用（可零提权驱动 TUN）。
    pub ready: bool,
    /// 可用但有新版（proto ≥ 最低可用但 < 期望）→ 温和提示可升级。
    pub upgradeable: bool,
    /// 协议版本（ping/version 返回），未就绪为缺省。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// 当前 app 期望的 shared protocol 版本；前端不得再硬编码版本数字。
    pub expected_protocol_version: u32,
    /// 已安装 helper 自报的构建身份；旧 helper 缺省（这本身就是可升级证据）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub helper_build_id: Option<String>,
    /// 当前 app 随包 helper 的期望构建身份，供诊断与 app/helper 同包对账。
    pub expected_build_id: String,
    /// daemon 是否被 launchd/systemd/SCM 加载；本层不主动探（避免每次 status spawn 进程）→ 恒 None（真机门）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded: Option<bool>,
    /// 已安装但无法就绪（token 丢失 / proto 过旧）→ 建议重装修复。
    pub needs_repair: bool,
    /// macOS 后台项（BTM）被禁——真机门（TCC 保护，Linux/GUI 读不到）→ 恒 false。
    pub background_disabled: bool,
    /// macOS 打包版 plist 烧录 sing-box 路径 ≠ 当前 app——真机门 → 恒 false。
    pub path_mismatch: bool,
    /// plist 烧录的 sing-box 路径（诊断展示）——真机门 → 恒 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_singbox_path: Option<String>,
}

/// install/uninstall 结果信封（前端 `helperApi.install/uninstall` 期望 `{ success, error?, status }`）。
///
/// 提权三态（[`EscalationOutcome`]）在此表达：成功 = `success:true`；用户取消 / 脚本失败 = `success:false`
/// + `error` 文案。**外层仍是 `ApiResponse::ok`**（IPC 层不失败——用户取消提权是正常流程，非命令错误）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperActionResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: HelperStatusSnapshot,
}

/// 从 client 侧 [`HelperStatus`] 投影到前端快照（补 supported + macOS 真机门惰值）。
fn snapshot_from(status: &HelperStatus, supported: bool) -> HelperStatusSnapshot {
    HelperStatusSnapshot {
        supported,
        installed: status.installed,
        ready: status.ready,
        upgradeable: status.upgradeable,
        version: status.version,
        expected_protocol_version: polaris_helper_proto::proto_version::CURRENT,
        helper_build_id: status.build_id.clone(),
        expected_build_id: polaris_helper_proto::build_identity::current().to_owned(),
        loaded: None,
        needs_repair: status.needs_repair,
        background_disabled: false,
        path_mismatch: false,
        installed_singbox_path: None,
    }
}

/// 平台 → 是否有提权 helper 实现（纯映射，全变体可单测）。mac/win/linux 三平台均有 helper
/// （对齐 `proxy.rs::should_start_via_helper` 的平台门：三平台 TUN 一律经 helper 起核）；`Other`
/// （freebsd/…）无对应实现。
///
/// 抽为自由 `const fn` 是为让**全 `Platform` 变体**在单一平台 gate 上可断言——给 mac/win 值以变异
/// 牙齿（`supported()` 读 `Platform::current()`，本机 gate 只走 Linux 一路，测不到 mac/win 逃逸面）。
const fn platform_supported(platform: Platform) -> bool {
    matches!(platform, Platform::Mac | Platform::Win | Platform::Linux)
}

/// 卸载 helper 之前该不该先停核（纯判定，可穷举单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallPreflight {
    /// 代理正**经 helper** 运行 → 先用「仍在的 helper」零提权停核，再卸载。
    StopCoreFirst,
    /// 代理未运行，或核是 app 自己直起的（不经 helper）→ 直接卸载。
    ProceedDirectly,
}

/// 纯判定：`proxy_running && started_via_helper` 才需要前置停核。
///
/// # 为什么两个条件都要，缺一不可
///
/// - `proxy_running`：没核在跑就没什么可停，多发一次 stop 只是噪音。
/// - `started_via_helper`：核若是 app 自己直起的（非 TUN 路径），它不归 daemon 管，
///   卸载 helper 不会让它变成孤儿；此时停核等于无故断用户的网。
///
/// 契约锚点：`~/docs/polaris/design/polaris-上游-capability-contract.md:93`「卸载前零提权停核」
/// + 上游 `helper-handlers.ts:54`（`getStatus().running && isStartedViaHelper()`）。
///
/// # 不停会怎样（这条腿存在的理由）
///
/// TUN 跑着时直接卸 helper：daemon 连同 socket 一起消失，而它拉起的 **root/SYSTEM 受管核**
/// 还在跑并占着 TUN。此后 app 再想停它，用户态 `kill` 收 EPERM 杀不动，只能落 forceKill 裸弹
/// 一次没有任何引导的 osascript —— 用户看到的是「卸了个东西，然后全网断了，还冒出个要密码的框」。
#[must_use]
pub const fn decide_uninstall_preflight(
    proxy_running: bool,
    started_via_helper: bool,
) -> UninstallPreflight {
    if proxy_running && started_via_helper {
        UninstallPreflight::StopCoreFirst
    } else {
        UninstallPreflight::ProceedDirectly
    }
}

/// 卸载前置停核编排（`stop` 注入 → 可单测，不碰真代理）。返回三态 [`PreflightStopResult`]。
///
/// # 停不掉时**继续卸载**（与 `update_install` 的停代理腿刻意相反）
///
/// | 腿 | 停失败时 | 为什么 |
/// |---|---|---|
/// | `update_install`（`commands/updater.rs`） | **中止安装** | 带着跑着的核替换应用本体会留半死不活态；更新可以稍后重试，代价只是晚点更新 |
/// | 本腿（卸载 helper） | **继续卸载** | 卸载是用户明确要求的终态动作。中止的话用户卡在「helper 卸不掉」且核照样跑着 —— 既没卸成、也没停成，比只丢一次停核更糟 |
///
/// 逐字对齐 上游 `helper-handlers.ts:55` 的 `await proxyManager.stop().catch(() => {})`。
/// 失败记 `warn`（不静默）：卸载完成后可能残留一个无人管的 root 核，日志得能对上账。
///
/// # 「停不掉之后怎么办」由**调用方**定，本函数只如实报告
///
/// 上表那条「继续卸载」是 **helper 单卸载**这条腿的决策，而不是本函数的。完全卸载
/// （`commands::app_uninstall_all`）后面还要删配置、删应用本体，停不掉核就必须整体中止
/// （理由见 [`crate::runtime::uninstall::stop_core_outcome`]）。故返回值从早先的 `bool`
/// 换成 [`PreflightStopResult`]：**两个调用方各自决定政策，判定与停核动作仍只有这一份**。
pub async fn uninstall_preflight_stop<F, Fut>(
    proxy_running: bool,
    started_via_helper: bool,
    stop: F,
) -> PreflightStopResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if decide_uninstall_preflight(proxy_running, started_via_helper)
        == UninstallPreflight::ProceedDirectly
    {
        return PreflightStopResult::NotNeeded;
    }
    if let Err(e) = stop().await {
        log::warn!(
            "卸载 helper 前停核失败（{e}）：helper 单卸载仍继续 —— 卸载后可能残留 root 受管核，需手动确认"
        );
        return PreflightStopResult::StopFailed(e);
    }
    PreflightStopResult::Stopped
}

/// 前置停核的三态结果（政策留给调用方，见 [`uninstall_preflight_stop`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightStopResult {
    /// 判定为无需停核（没跑，或核不经 helper 起）。
    NotNeeded,
    /// 真发起了停核且成功。
    Stopped,
    /// 真发起了停核但失败，带原因。
    StopFailed(String),
}

impl PreflightStopResult {
    /// 是否真发起过停核（= 旧 `bool` 返回值的语义，给既有真值表用）。
    #[must_use]
    pub const fn attempted(&self) -> bool {
        !matches!(self, Self::NotNeeded)
    }

    /// 失败原因（成功/无需停核为 `None`）——完全卸载腿据此把停核映射成硬失败。
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::StopFailed(e) => Some(e),
            _ => None,
        }
    }
}

/// 「让 daemon 停掉它自己的受管 child」这一个能力的窄 trait。
///
/// **为什么要这层间接**：[`HelperRuntime`] 直连 [`InstallPaths`] 给的**系统路径** socket
/// （`/var/run/...`），单测既起不了真 daemon、也不许往系统路径写 —— 没有替身就等于起核收口腿
/// 完全无法被断言（「stop 有没有被调」只能靠读代码推理）。抽成窄 trait 后可注入可观测替身，
/// 把它变成有牙的门。
pub trait HelperStopOps: Send + Sync {
    /// 请 daemon 终止其受管 sing-box child。
    ///
    /// `want_pid` = **本腿意图停的那个 pid**（身份判据，见 [`HelperRuntime::stop_core`]）。
    /// `None` = 尚不知 pid（起核 IPC 在飞）→ 旧语义「停当前受管核」。
    fn stop_managed_core(&self, want_pid: Option<u32>) -> Result<(), String>;
}

impl HelperStopOps for HelperRuntime {
    fn stop_managed_core(&self, want_pid: Option<u32>) -> Result<(), String> {
        self.stop_core(want_pid)
    }
}

/// 完全卸载腿的注入面（窄 trait 的理由同 [`HelperStopOps`]：单测不许起 daemon、更不许弹提权框）。
///
/// 这里只做**信封转换**（`HelperActionResult` → `Result`）与**路径取值**，一条判定都不加 ——
/// 判定全在 [`crate::runtime::uninstall`] 的纯函数里。
impl crate::runtime::uninstall::HelperUninstallOps for HelperRuntime {
    fn supported(&self) -> bool {
        HelperRuntime::supported(self)
    }

    fn installed(&self) -> bool {
        self.status().installed
    }

    fn uninstall(&self) -> Result<(), String> {
        let r = HelperRuntime::uninstall(self);
        if r.success {
            Ok(())
        } else {
            // 提权三态里「用户取消」也落这里 —— 它对完全卸载就是一次失败（用户没授权 ⇒ 什么都没删），
            // 必须让 fail-fast 拦下后面的删除，而不是当成「跳过」继续删配置和应用本体。
            Err(r
                .error
                .unwrap_or_else(|| "卸载提权助手失败（无错误详情）".to_owned()))
        }
    }

    fn protected_core_dir(&self) -> String {
        let paths = InstallPaths::for_platform(self.platform);
        match self.platform {
            // win 不播种受管核（`InstallPaths::win().core_dir` 只是占位值，报它等于报了个假路径）；
            // 真正被 root 卸载脚本清掉的是 helper 支持目录。
            Platform::Win => paths.binary.parent().map_or_else(
                || "helper 支持目录".to_owned(),
                |p| {
                    format!(
                        "Windows 内核走应用侧，无受保护内核目录；已清除 {}",
                        p.display()
                    )
                },
            ),
            _ => paths.core_dir.display().to_string(),
        }
    }
}

/// [`SysOps`] 工厂（每次 [`HelperRuntime::manager`] 现造一个：`HelperManager` 要 `Box<dyn SysOps>`
/// 的所有权，而 `SysOps: Send` 不要求 `Sync`，存不成共享实例）。
type SysOpsFactory = Arc<dyn Fn() -> Box<dyn SysOps> + Send + Sync>;

/// helper 运行时（`State`-managed，单实例）。
pub struct HelperRuntime {
    dir: PathBuf,
    platform: Platform,
    /// 装/载探测的系统面。生产恒 [`StdSysOps`]；**单测必须注入替身**，理由见
    /// [`HelperRuntime::never_installed_for_tests`]。
    sys_ops: SysOpsFactory,
}

impl HelperRuntime {
    pub fn new(dir: PathBuf) -> Self {
        let platform = Platform::current();
        Self {
            dir,
            platform,
            sys_ops: Arc::new(|| Box::new(StdSysOps)),
        }
    }

    /// **测试专用**构造：`SysOps` 替身恒报「二进制/描述符/服务都不存在」⇒ [`Self::status`] 稳定判未装。
    ///
    /// # 为什么必须有这个
    ///
    /// 生产 [`Self::new`] 走 [`StdSysOps`] + [`InstallPaths::for_platform`] 的**系统路径**，于是
    /// `status()` 读的是**宿主真实安装态**。原先所有相关单测都默认「本机/CI 从不装 polaris-helper」，
    /// 这个前提一旦破（开发机上真装过一次 Polaris），后果不是「少测一点」，而是：
    ///
    /// 1. 12 条门当场转红（2026-07-28 在 5.238 实测，那台装过 Polaris）；
    /// 2. 更糟：`installed=true` 让 `compute_status_with_client` 的短路失效，单测**真的去连
    ///    `/Library/.../polaris-helper.sock` 这个特权 daemon**并发起 `start` —— 实测拿到 `ERR auth`
    ///    才没造成副作用，即「没起成核」靠的是 token 不匹配，不是靠设计。
    ///
    /// 即门的绿与否取决于跑测机器的状态，而非被测代码 —— 换台机器就换结论。注入替身后，
    /// 「不触碰系统 socket」从**巧合**变成**结构保证**。
    #[cfg(test)]
    pub(crate) fn never_installed_for_tests(dir: PathBuf) -> Self {
        struct NeverInstalled;
        impl SysOps for NeverInstalled {
            fn exists(&self, _path: &Path) -> bool {
                false
            }
            fn start_service(&self, _label: &str) -> Result<(), String> {
                Err("测试替身：不碰真实服务".to_owned())
            }
            fn stop_service(&self, _label: &str) -> Result<(), String> {
                Err("测试替身：不碰真实服务".to_owned())
            }
            fn is_loaded(&self, _label: &str) -> bool {
                false
            }
            fn service_exists(&self, _label: &str) -> bool {
                false
            }
        }
        Self {
            dir,
            platform: Platform::current(),
            sys_ops: Arc::new(|| Box::new(NeverInstalled)),
        }
    }

    /// 当前平台。
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// 配置目录（helper token 落 `<dir>/helper-client.token`）。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 当前平台是否有提权 helper 实现（mac/win/linux 三平台均有；未知平台无）。
    #[must_use]
    pub const fn supported(&self) -> bool {
        platform_supported(self.platform)
    }

    /// app 侧 token 文件路径。
    fn token_path(&self) -> PathBuf {
        self.dir.join(HELPER_TOKEN_FILE)
    }

    /// 构造生命周期管理器（生产 [`StdSysOps`]：真 launchctl/systemctl/sc + `fs::exists`；
    /// 单测经 [`Self::never_installed_for_tests`] 注入替身）。
    fn manager(&self) -> HelperManager {
        HelperManager::new(self.platform, self.token_path(), (self.sys_ops)())
    }

    /// 构造生产 [`HelperClient`]（每请求一连接，经生产 Connector）。
    ///
    /// **不实际连接**：`connect()` 延迟到 `send` 时——未装/未跑时 `send` 才返 `Connect` 错误（对齐
    /// 上游 `net.connect` 的惰性）。故 `status` 在未安装态（`compute_status_with_client` 先判 is_installed）
    /// **绝不触碰 socket** → 本机安全。
    fn build_client(&self) -> Result<HelperClient, String> {
        let paths = InstallPaths::for_platform(self.platform);
        let token = read_token(&self.token_path());
        let connector = Self::connector(&paths.socket)?;
        Ok(HelperClient::new(connector, self.platform, token))
    }

    #[cfg(unix)]
    fn connector(socket: &Path) -> Result<Box<dyn Connector>, String> {
        Ok(Box::new(UnixConnector::new(socket)))
    }

    #[cfg(windows)]
    fn connector(socket: &Path) -> Result<Box<dyn Connector>, String> {
        Ok(Box::new(PipeConnector::new(socket)))
    }

    #[cfg(not(any(unix, windows)))]
    fn connector(_socket: &Path) -> Result<Box<dyn Connector>, String> {
        Err("当前平台无 helper connector".to_owned())
    }

    /// 状态快照（上游 `helper:getStatus`）——真探测：`status_with_recovery`（is_installed 短路、
    /// ping proto、W20 恢复腿——「装了但停着」先拉起复核，不误报修复态）。未安装 → 不连 socket
    /// 直接返未装态。恢复腿让所有 status 消费方共享同一自愈：设置页挂载、启动 7s 可升级探测、
    /// 手动重新检测。
    ///
    /// 已知取舍：卸载读门（`HelperUninstallOps::installed`）也走本方法——用户要卸载时若服务正停着，
    /// 读门会先把服务拉起、随后又被卸载流程停/删（无害但多付一次恢复耗时）。UI 读门一律异步
    /// （`helper_get_status` spawn_blocking），不冻 UI。
    #[must_use]
    pub fn status(&self) -> HelperStatusSnapshot {
        let supported = self.supported();
        if !supported {
            return HelperStatusSnapshot {
                supported,
                ..Default::default()
            };
        }
        let manager = self.manager();
        match self.build_client() {
            Ok(client) => snapshot_from(&manager.status_with_recovery(&client), supported),
            Err(e) => {
                log::debug!("helper status：建 client 失败（{e}）→ 视作未就绪");
                snapshot_from(&HelperStatus::default(), supported)
            }
        }
    }

    /// 组装 install 结果（失败态复用一次真状态探测）。
    fn action_failed(&self, error: impl Into<String>) -> HelperActionResult {
        HelperActionResult {
            success: false,
            error: Some(error.into()),
            status: self.status(),
        }
    }

    /// 安装/修复 helper（上游 `helper:install`）——真流程：HelperManager.install（拷二进制 + 写 root token
    /// + 播种核 + 写描述符 + bootstrap，经提权跑一次 root 脚本）。返回三态信封。
    ///
    /// **真机门**：成功路径触发 osascript/UAC/pkexec 弹框。本机无 bundled helper → 早返「二进制缺失」，不弹框。
    #[must_use]
    pub fn install(&self) -> HelperActionResult {
        if !self.supported() {
            return self.action_failed("当前平台不支持提权 helper");
        }
        let params = match self.install_params() {
            Ok(p) => p,
            Err(e) => return self.action_failed(e),
        };
        let manager = self.manager();
        match manager.install(&params, &StdExecutor) {
            Ok(EscalationOutcome::Success) => {
                // 装完轮询就绪（daemon 注册后绑 socket 需时间）。
                let status = match self.build_client() {
                    Ok(client) => snapshot_from(
                        &manager.wait_until_ready(&client, READY_POLL_ATTEMPTS, READY_POLL_DELAY),
                        true,
                    ),
                    Err(_) => self.status(),
                };
                HelperActionResult {
                    success: true,
                    error: None,
                    status,
                }
            }
            Ok(EscalationOutcome::Cancelled) => self.action_failed("已取消管理员授权"),
            Ok(EscalationOutcome::Failed { stderr, code }) => {
                self.action_failed(format!("安装失败（退出码 {code}）：{stderr}"))
            }
            Err(e) => self.action_failed(format!("安装失败：{e}")),
        }
    }

    /// 卸载 helper（上游 `helper:uninstall`）——真流程：HelperManager.uninstall（bootout/删描述符/删二进制/
    /// 删受保护目录，经提权跑）。返回三态信封。
    ///
    /// ⚠️ **前置停核不在本方法内**：本运行时不持有 `ProxyRuntime`（那是 `AppRuntime` 层的编排）。
    /// 「卸载前零提权停核」由命令层 [`crate::commands::helper_uninstall`] 经
    /// [`uninstall_preflight_stop`] 完成 —— 直接调本方法会跳过那道闸。
    #[must_use]
    pub fn uninstall(&self) -> HelperActionResult {
        if !self.supported() {
            return self.action_failed("当前平台不支持提权 helper");
        }
        let manager = self.manager();
        match manager.uninstall(&self.dir, &StdExecutor) {
            Ok(EscalationOutcome::Success) => HelperActionResult {
                success: true,
                error: None,
                status: self.status(),
            },
            Ok(EscalationOutcome::Cancelled) => self.action_failed("已取消管理员授权"),
            Ok(EscalationOutcome::Failed { stderr, code }) => {
                self.action_failed(format!("卸载失败（退出码 {code}）：{stderr}"))
            }
            Err(e) => self.action_failed(format!("卸载失败：{e}")),
        }
    }

    /// 组装 [`InstallParams`]（解析 bundled 资源 + 当前 uid）。
    fn install_params(&self) -> Result<InstallParams, String> {
        let src_binary = resolve_helper_binary()?;
        // mac/linux 播种 root 受管核；win 用它作 `--singbox`（app 侧核）。
        let bundled_core = crate::runtime::proxy::resolve_core_binary()?;
        Ok(InstallParams {
            src_binary,
            bundled_core: bundled_core.clone(),
            singbox_path: bundled_core,
            conf_dir: self.dir.clone(),
            uid: current_uid(),
            script_dir: self.dir.clone(),
        })
    }

    /// 经就绪 helper 提权起核（TUN 模式，proxy.rs 决策路由后调）。返回 root/SYSTEM 受管核 pid。
    ///
    /// - linux：`Request::LinuxStart`（多带核路径行，helper 校验 == 锁定 coreBin + setuid 回登录用户 +
    ///   AmbientCaps 拉核）。
    /// - mac/win：`Request::Start`（helper 用锁定的 `--singbox` 核）。
    ///
    /// # 为什么**不收**「要跑哪个核」这个参数（根因）
    ///
    /// 本方法起的核，其二进制**由 helper 单方面决定**：mac/win 的 `start` 协议压根没有核路径字段
    /// （`request.rs:35-45`），helper 恒 exec 安装期 `--singbox` 锁定的那一个；linux 虽有该字段，
    /// 但 helper 会强制它 == 锁定的 `coredir/sing-box`，否则 `ERR core-path-denied`
    /// （`platform/linux/handler.rs:350`）。这是**安全边界**（杜绝「持 token 让 root 跑任意二进制」）。
    ///
    /// 早先本方法收一个 `singbox: &Path`，调用方老老实实传了 app 解析出的现役核，而 mac 分支
    /// **把它整个丢掉** —— 于是「app 请求的 bin」与「helper 实际跑的 bin」可以长期分叉且零告警
    /// （p101 实测：请求 `core_update/sing-box`(1.14.0-beta.3)，实跑受保护核 1.14.0-alpha.45，持续一天多）。
    /// 参数留着就是在邀请下一个调用方再次误以为「我传什么它就跑什么」，故**删掉**：
    /// 想让 helper 跑新核只有一条路 —— [`install_core`](Self::install_core) 换掉锁定路径的**内容**。
    /// linux 分支据此改传[受保护核路径](Self::protected_core_dir_path)（即 helper 真会跑的那个），
    /// 而非 app 侧可写核路径 —— 后者必被 helper 判 `core-path-denied`。
    ///
    /// **真机门**：真起 root 受管核 + 建 TUN。`fwd` = allowLan（开 IP 转发），`ppid` = app pid（父死看护）。
    pub fn start_core(
        &self,
        cfg: &Path,
        log: &Path,
        fwd: bool,
        ppid: Option<u32>,
    ) -> Result<u32, String> {
        let client = self.build_client()?;
        let common = StartParams {
            cfg: cfg.to_string_lossy().into_owned(),
            log: log.to_string_lossy().into_owned(),
            fwd,
            parent_pid: ppid,
        };
        let req = match self.platform {
            Platform::Mac | Platform::Win => Request::Start(common),
            // linux/未知谱系：带核路径行，且**只能**是 helper 锁定的 coreBin（它会逐字比对）。
            Platform::Linux | Platform::Other => Request::LinuxStart(LinuxStartParams {
                singbox_path: crate::runtime::core_promote::protected_core_path_in(
                    &self.protected_core_dir_path(),
                    std::env::consts::OS,
                )
                .to_string_lossy()
                .into_owned(),
                common,
            }),
        };
        let resp = client
            .send_with_timeout(&req, HELPER_START_TIMEOUT)
            .map_err(|e| format!("helper 起核通信失败：{e}"))?;
        match resp {
            Response::Ok(ResponseKind::Start(Start::StartedTimed { pid, timing })) => {
                log::info!(
                    "helper core start timing: forwarding={}ms process={}ms job={}ms log_handoff={}ms total={}ms",
                    timing.forwarding_ms,
                    timing.process_ms,
                    timing.job_ms,
                    timing.log_handoff_ms,
                    timing.total_ms
                );
                Ok(pid)
            }
            Response::Ok(ResponseKind::Start(Start::Started { pid } | Start::Already { pid })) => {
                Ok(pid)
            }
            Response::Ok(other) => Err(format!("helper 起核返回非预期响应：{other:?}")),
            Response::Err(e) => Err(format!("helper 起核失败：{e}")),
        }
    }

    /// **受保护核目录**（mac/linux 的 root 锁定核目录；win 无此概念，见
    /// [`core_promote::platform_has_protected_core`](crate::runtime::core_promote::platform_has_protected_core)）。
    ///
    /// 与 helper 安装期烧进 plist/unit 的 `--coredir` 同源（[`InstallPaths::for_platform`]），
    /// 故它就是 helper `--singbox` 所指那个文件的父目录 —— 「app 认为 helper 会跑哪个文件」
    /// 与「helper 实际跑哪个文件」由这一个真值保证同步。
    #[must_use]
    pub fn protected_core_dir_path(&self) -> PathBuf {
        InstallPaths::for_platform(self.platform).core_dir
    }

    /// 经 helper 把暂存目录里的核 root 写入受保护核目录（`install-core`）。
    ///
    /// **这是「换核对 TUN 提权路径生效」的唯一通道**：mac/win 的 `start` 不带核路径、linux 的会被
    /// 强制校验成锁定路径，故新核只能靠本命令**换掉那个锁定路径的内容**（路径不变 ⇒ helper 无需重启）。
    /// 移植自 上游 `HelperManager.installCore`（`HelperManager.ts:421-430`）——helper 侧
    /// （[`polaris_helper::core_install::install_core_files`]）在 Polaris 移植时就已完整落地，
    /// 缺的一直是本方法这条 app 侧调用边。
    ///
    /// `src_dir` 必须是**只含该进受保护目录的文件**的干净目录：helper 会把其中每个非目录文件都搬进去，
    /// 并把不在其中的旧文件 prune 掉（`core_install::prune_extra_files`）。备好它用
    /// [`core_promote::stage_promote_dir`](crate::runtime::core_promote::stage_promote_dir)。
    ///
    /// `want_hash` = `src_dir/sing-box` 的 sha256 hex（helper 读全字节复算比对，堵 TOCTOU）。
    ///
    /// **真机门**：真写 root 目录需 mac/win 真机 + 已就绪 helper。
    ///
    /// # Errors
    ///
    /// 建客户端失败 / IPC 失败 / helper 返回 `ERR *`（`hash-mismatch` / `coredir-unset` / 写盘失败等）。
    pub fn install_core(&self, src_dir: &Path, want_hash: &str) -> Result<(), String> {
        let client = self.build_client()?;
        let req = Request::InstallCore(InstallCoreParams {
            src_dir: src_dir.to_string_lossy().into_owned(),
            want_hash: want_hash.to_owned(),
        });
        // install-core 走长超时（sha256 + 80MB 量级复制），沿用 client 侧既有常量，不另开一个。
        let resp = client
            .send_with_timeout(&req, Duration::from_millis(INSTALL_CORE_TIMEOUT_MS))
            .map_err(|e| format!("helper 装核通信失败：{e}"))?;
        match resp {
            Response::Ok(ResponseKind::Installed) => Ok(()),
            Response::Ok(other) => Err(format!("helper 装核返回非预期响应：{other:?}")),
            Response::Err(e) => Err(format!("helper 装核失败：{e}")),
        }
    }

    /// 经 helper 停核（对称：经 helper 起的核经 helper stop）。daemon `stop` 摘其受管 child → 终止 →
    /// 收割。
    ///
    /// **`want_pid` 是身份判据，不是可选诊断字段**（根因）：这条腿是同步阻塞 IPC —— 从发出 `stop`
    /// 到 daemon 真动手之间可以隔很久（socket 已删/daemon 无响应时更久）。这期间用户完全可能重装
    /// helper 并起了新核，daemon 手里的「受管 pid」此刻已经换成**新核**。若不带身份，daemon 按
    /// 「停当前受管的」执行 = 杀掉用户刚连上的核（表现为「刚连上就被静默断开」，且酷似核自己崩了）。
    /// app 侧的世代守卫够不着这一层：杀进程发生在 helper 进程里，故判据必须随请求下发。
    ///
    /// `None` = 调用点确实还不知道 pid（起核 IPC 在飞、pid 未回传）→ 旧语义「停当前受管核」，这是
    /// 防 root 孤儿所必需的（见 `proxy.rs::spawn_core_via_helper` 的孤儿不变式）。
    ///
    /// 身份不匹配 → daemon 回 `stop-mismatch` 且**一个进程都不杀**；本方法把它落成 `Err`（连同两个
    /// pid），因为「本腿意图停的核」确实没被停 —— 报 `Ok` 会让调用方的日志说谎。
    ///
    /// **真机门**：真停 root/SYSTEM 受管核。
    pub fn stop_core(&self, want_pid: Option<u32>) -> Result<(), String> {
        let client = self.build_client()?;
        let resp = client
            .send(&Request::Stop { pid: want_pid })
            .map_err(|e| format!("helper 停核通信失败：{e}"))?;
        match resp {
            Response::Ok(ResponseKind::Stop(Stop::Mismatch { want, current })) => Err(format!(
                "helper 未停核：其受管核已是 pid={current}（本腿意图停 pid={want}）\
                 → 判定为已被新会话接管，让位不动它"
            )),
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(format!("helper 停核失败：{e}")),
        }
    }

    /// **T3 提权清扫**：经 helper 清掉 root 起的孤儿核（用户态 `kill` 收 EPERM 杀不动的那些）。
    ///
    /// daemon 侧 `Cleanup` = `pkill -9 -f "<锁定的 singbox_bin> run"` + 摘 child——**模式锚在
    /// helper 自己锁定的核路径上**，故不会误杀用户系统装的无关 sing-box（与 [`stop_core`](Self::stop_core)
    /// 只停受管 child 不同：此处要清的正是 daemon 已不再持有句柄的历史遗留）。
    ///
    /// **调用约束**：它是无差别清扫（连当前受管核一起杀），故**只允许在起核前的清扫期调用**
    /// （`cleanup_stale_cores`，此时本会话尚无受管核）。运行期停核走 `stop_core`。
    ///
    /// **真机门**：真杀 root 进程需 mac/win 真机 + 已装就绪 helper。
    pub fn cleanup_cores(&self) -> Result<(), String> {
        let client = self.build_client()?;
        let resp = client
            .send(&Request::Cleanup)
            .map_err(|e| format!("helper 清扫通信失败：{e}"))?;
        match resp {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(format!("helper 清扫失败：{e}")),
        }
    }

    /// C5：经 root/SYSTEM helper 装/删 mesh 出口路由（mac：`route add/del -ifscope`；win：`route add/del`）。
    /// `op`="add"|"del"。**Linux 不经此腿**——app 自身 CAP_NET_ADMIN 直接 `ip route`（独立表 7732 + oif 规则，
    /// 见 `runtime/mesh.rs::HelperExitRouteOp`）；helper 协议蓄意无 Linux route 命令。
    ///
    /// 契约（对齐 上游 `MeshExitRouteManager.runRoute` 的 best-effort）：**永不 panic** —— 建 client /
    /// 通信 / 协议 / helper 侧失败一律收敛为 `false`，调用方（`HelperExitRouteOp::run_route`）据此不标 installed。
    ///
    /// **真机门**：真改宿主路由表需 mac/win 真机 + 已装就绪 helper；本机（Linux）不经此腿。
    #[must_use]
    pub fn route_op(&self, op: &str, iface: &str, cidrs: &[String]) -> bool {
        let client = match self.build_client() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("出口路由 helper：建 client 失败（{e}）→ route-{op} 失败");
                return false;
            }
        };
        let params = RouteParams {
            iface: iface.to_string(),
            cidrs: cidrs.to_vec(),
        };
        let req = if op == "add" {
            Request::RouteAdd(params)
        } else {
            Request::RouteDel(params)
        };
        match client.send_with_timeout(&req, HELPER_ROUTE_TIMEOUT) {
            Ok(Response::Ok(ResponseKind::Route)) => true,
            Ok(other) => {
                log::warn!("helper route-{op} 返回非预期响应（可能 helper 版本过旧）：{other:?}");
                false
            }
            Err(e) => {
                log::warn!("helper route-{op} 通信失败：{e}");
                false
            }
        }
    }

    /// C7：经 root/SYSTEM helper 刷系统 DNS 缓存（mac `flush-dns`：dscacheutil + HUP mDNSResponder 两层全清）。
    ///
    /// 契约（对齐 上游 `HelperManager.flushDns`）：**永不抛** —— 通信/协议失败收敛为 `ok:false`，调用方
    /// （`dns_flush::flush_os_dns_cache`）据此降级用户级 `dscacheutil`。`partial`=dscacheutil 成功但 HUP
    /// mDNSResponder 失败（app 不降级，用户级同样无权 HUP）。5s 超时。
    ///
    /// **真机门**：真正的 root helper `killall -HUP mDNSResponder` 需 mac 真机 + 已装 helper；本机 Linux
    /// 不经此腿（`flush_os_dns_cache` 仅 mac 调 helper，见其平台分派）。
    #[must_use]
    pub fn flush_dns(&self) -> HelperFlushResult {
        let client = match self.build_client() {
            Ok(c) => c,
            Err(e) => {
                return HelperFlushResult {
                    ok: false,
                    partial: None,
                    error: Some(e),
                }
            }
        };
        match client.send_with_timeout(&Request::FlushDns, HELPER_FLUSH_TIMEOUT) {
            Ok(Response::Ok(ResponseKind::FlushDns(FlushDns::Flushed))) => HelperFlushResult {
                ok: true,
                partial: None,
                error: None,
            },
            Ok(Response::Ok(ResponseKind::FlushDns(FlushDns::FlushedPartial { tail }))) => {
                HelperFlushResult {
                    ok: true,
                    partial: Some(tail),
                    error: None,
                }
            }
            Ok(Response::Ok(other)) => HelperFlushResult {
                ok: false,
                partial: None,
                error: Some(format!("helper flush-dns 返回非预期响应：{other:?}")),
            },
            Ok(Response::Err(e)) => HelperFlushResult {
                ok: false,
                partial: None,
                error: Some(e.to_string()),
            },
            Err(e) => HelperFlushResult {
                ok: false,
                partial: None,
                error: Some(format!("helper flush-dns 通信失败：{e}")),
            },
        }
    }
}

/// 当前登录用户 uid（linux install 写 `authorized-uids`；mac/win InstallParams 忽略）。
///
/// 经 `/proc/self` 属主读取（stdlib `MetadataExt::uid`，无新依赖、无 unsafe）——`/proc/self` 恒属真实
/// 运行用户。非 Linux（无 `/proc`）→ 回退 0（该平台 InstallParams 忽略 uid，无害）。
#[cfg(unix)]
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map_or(0, |m| m.uid())
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// 解析 bundled `polaris-helper` 二进制（镜像 [`resolve_core_binary`](crate::runtime::proxy::resolve_core_binary)
/// 的前缀/平台目录策略）。找不到 → Err（install 据此早返「二进制缺失」，**不触发提权**）。
fn resolve_helper_binary() -> Result<PathBuf, String> {
    let filename = if cfg!(windows) {
        "polaris-helper.exe"
    } else {
        "polaris-helper"
    };
    if let Ok(p) = std::env::var("POLARIS_HELPER_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "POLARIS_HELPER_PATH 指向的文件不存在：{}",
            p.display()
        ));
    }
    let platform_dirs: &[&str] = if cfg!(target_os = "macos") {
        &["mac-arm64", "mac-x64"]
    } else if cfg!(windows) {
        &["win"]
    } else {
        &["linux"]
    };
    // 与 resolve_core_binary 共用同一布局兜底（含 macOS `_up_` 真实路径），钉在 proxy.rs 的纯函数里。
    let exe = std::env::current_exe().ok();
    let candidates = crate::runtime::proxy::bundle_resource_candidates(
        exe.as_deref().and_then(std::path::Path::parent),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        platform_dirs,
        filename,
    );
    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "未找到 polaris-helper 二进制（尝试过：{}）",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    /// 唯一临时目录（无 tempfile 依赖；用完 remove_dir_all）。
    struct TestDir(PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn runtime() -> (HelperRuntime, TestDir) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("polaris-helper-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // **不用 `HelperRuntime::new`**：那条走 StdSysOps + 系统路径，会把门的绿绑在「跑测的机器上
        // 没装过 Polaris」这个宿主前提上（且装了就会真连特权 daemon）。见 `never_installed_for_tests`。
        (
            HelperRuntime::never_installed_for_tests(dir.clone()),
            TestDir(dir),
        )
    }

    #[test]
    fn platform_supported_maps_all_platforms() {
        // 三平台均有提权 helper → 卡片可达（deriveState 的 `!s.supported` 不再恒真，前端落 installed/
        // none/needs-* 真实态而非 unsupported）。全变体断言给 mac/win/linux 值变异牙齿：任何漏平台的
        // 变异（如 `Mac | Win`、`Mac | Linux`、`Win | Linux`）在本机 Linux gate 即失败，无需真机跨平台。
        assert!(
            platform_supported(Platform::Mac),
            "macOS 有提权 helper（LaunchDaemon）"
        );
        assert!(
            platform_supported(Platform::Win),
            "Windows 有提权 helper（SCM）"
        );
        assert!(
            platform_supported(Platform::Linux),
            "Linux 有提权 helper（systemd + AmbientCaps；对齐 should_start_via_helper）"
        );
        assert!(
            !platform_supported(Platform::Other),
            "未知平台无 helper 实现 → unsupported 正确"
        );
    }

    #[test]
    fn status_supported_reflects_platform() {
        let (rt, _d) = runtime();
        // supported 随平台；未装（替身恒报不存在）→ 其余全 false（compute_status 先判 is_installed
        // 短路，不连 socket）。**未装态由注入的替身给定，不再取决于跑测机器装没装过 Polaris。**
        let s = rt.status();
        assert_eq!(s.supported, cfg!(any(unix, windows)));
        assert!(!s.installed, "替身报不存在 → not installed");
        assert!(!s.ready);
        assert!(!s.needs_repair, "未安装 ≠ needs_repair");
    }

    #[test]
    fn status_serializes_frontend_keys() {
        let (rt, _d) = runtime();
        let json = serde_json::to_value(rt.status()).unwrap();
        // 前端 deriveState 消费的键必须在位（camelCase）。
        for key in [
            "supported",
            "installed",
            "ready",
            "upgradeable",
            "expectedProtocolVersion",
            "expectedBuildId",
            "needsRepair",
            "backgroundDisabled",
            "pathMismatch",
        ] {
            assert!(json.get(key).is_some(), "缺前端契约键 {key}: {json}");
        }
    }

    #[test]
    fn install_missing_binary_returns_failure_without_escalation() {
        // 无 bundled polaris-helper（且未设 POLARIS_HELPER_PATH）→ install 早返失败，绝不弹提权框。
        // 尾部 `r.status.installed` 由替身给定（见 `runtime()`），不再赖「跑测机器没装过」。
        std::env::remove_var("POLARIS_HELPER_PATH");
        let (rt, _d) = runtime();
        let r = rt.install();
        assert!(!r.success, "缺二进制必失败");
        assert!(r.error.is_some());
        // 状态仍是真探测（未安装）。
        assert!(!r.status.installed);
    }

    #[test]
    fn action_result_serializes_success_error_status() {
        let (rt, _d) = runtime();
        let json = serde_json::to_value(rt.install()).unwrap();
        assert!(json.get("success").is_some());
        assert!(
            json.get("status").is_some(),
            "install 结果须含 status（前端 r.status 消费）"
        );
    }

    // ── 卸载前置停核（契约 §93「卸载前零提权停核」）─────────────────────────────

    #[test]
    fn uninstall_preflight_truth_table() {
        use UninstallPreflight::{ProceedDirectly, StopCoreFirst};
        // 唯一该停的组合：跑着 **且** 经 helper 起。
        assert_eq!(decide_uninstall_preflight(true, true), StopCoreFirst);
        // 没跑 → 无核可停。
        assert_eq!(decide_uninstall_preflight(false, true), ProceedDirectly);
        // 跑着但 app 直起（非 TUN）→ 不归 daemon 管，停它等于无故断网。
        assert_eq!(decide_uninstall_preflight(true, false), ProceedDirectly);
        assert_eq!(decide_uninstall_preflight(false, false), ProceedDirectly);
    }

    /// 🟡 **变异锁：TUN 跑着时卸 helper，stop 必须真被调。**
    ///
    /// 把 `commands::helper_uninstall` 里的 `uninstall_preflight_stop` 删掉 ⇒
    /// `commands::helper` 的调用点守卫转红；把本函数体里的 `stop().await` 删掉 ⇒ 本条转红。
    #[test]
    fn preflight_calls_stop_only_when_core_runs_via_helper() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rt = tokio::runtime::Runtime::new().unwrap();

        let calls = AtomicUsize::new(0);
        let r = rt.block_on(uninstall_preflight_stop(true, true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        assert!(r.attempted(), "TUN 经 helper 跑着 → 必须先停核");
        assert_eq!(r, PreflightStopResult::Stopped);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 未运行 → 一次都不调。
        let calls = AtomicUsize::new(0);
        let r = rt.block_on(uninstall_preflight_stop(false, true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        assert!(!r.attempted());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // 非经 helper 起 → 一次都不调（不无故断用户的网）。
        let calls = AtomicUsize::new(0);
        rt.block_on(uninstall_preflight_stop(true, false, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// 停核失败**如实上报原因**，且本函数不替调用方做「中止还是继续」的决定。
    ///
    /// 与 `update_install` 的停代理腿刻意相反：helper 单卸载停不掉也继续卸（见函数文档的表），
    /// 「继续」在此表达为函数正常返回、不 panic、不把失败当中止信号往上抛。而完全卸载腿会读
    /// [`PreflightStopResult::error`] 把它映射成硬失败 —— 两种政策共用同一份判定与停核动作。
    ///
    /// **变异探针**：把 `StopFailed(e)` 腿改回吞掉错误（返 `Stopped`）⇒ 本条转红，且
    /// `runtime::uninstall` 的 `stop_core_failure_blocks_every_delete` 会失去输入源。
    #[test]
    fn preflight_stop_failure_is_reported_not_swallowed() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(uninstall_preflight_stop(true, true, || async {
            Err("mock: helper socket 已消失".to_string())
        }));
        assert!(
            r.attempted(),
            "停核失败仍须算「已尝试」—— helper 单卸载据此照常推进"
        );
        assert_eq!(
            r.error(),
            Some("mock: helper socket 已消失"),
            "原因必须原样带出：完全卸载腿要拿它当中止理由报给用户"
        );
    }

    #[test]
    fn resolve_helper_binary_env_override_rejects_missing() {
        std::env::set_var("POLARIS_HELPER_PATH", "/nonexistent/polaris-helper-xyz");
        let r = resolve_helper_binary();
        std::env::remove_var("POLARIS_HELPER_PATH");
        assert!(r.is_err());
    }

    /// W20 防回潮：`status()` 必须走带恢复腿的探测。直连 `compute_status_with_client` 会把
    /// 「装了但停着」误报成修复态（UI 弹「修复助手」），而那只是 `sc start` 一把的事。
    /// 行为已在 helper-client 单测覆盖（recovery_* 五条），这里源码级钉住接线不被回退——
    /// 本机 Linux gate 走不到 win 分身，编译器拦不住这行被改回去。
    ///
    /// 钉法（W17 先例）：从 `pub fn status(` 切片到下一个兄弟文档注释，只断言生产函数体；
    /// 断言串再经 `concat!` 打断——否则测试自身的字符串字面量就是 include_str 里的一个
    /// 命中点，改回生产代码后测试照样绿（评审 F1 实证过的假绿形态）。
    #[test]
    fn status_wiring_uses_recovery_probe() {
        let src = include_str!("helper.rs");
        let start = src.find("pub fn status(").expect("status 消失");
        let rest = &src[start..];
        let end = rest.find("\n    /// ").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains(concat!("status_with_recovery", "(&client)")),
            "HelperRuntime::status 必须调 status_with_recovery（W20 恢复腿）"
        );
    }
}
