//! [`HelperManager`] —— helper 安装/卸载/启动/停止生命周期管理。
//!
//! ## 职责（移植自 上游 `HelperManager.ts`）
//!
//! 三平台 helper 的**生命周期决策**：判定是否已装/就绪、装/卸路径、proto 版本探测。
//! 实际系统操作（launchd bootstrap / systemd enable / SCM install）经 [`SysOps`] trait 抽象，
//! 生产注入真 syscall，测试 mock —— 满足「不触碰宿主」纪律。
//!
//! ## 三平台安装路径（移植锚点）
//!
//! - **macOS**：`/Library/PrivilegedHelperTools/com.polaris.helper`（二进制）+
//!   `/Library/LaunchDaemons/com.polaris.helper.plist`（plist）+ launchd bootstrap。
//!   （上游 `HelperManager.ts:30,33-34`）
//! - **Linux**：`/usr/local/lib/polaris/helper`（二进制）+ `polaris-helper.service`（systemd unit）+ systemctl。
//! - **Windows**：`C:\Program Files\Polaris\helper.exe`（二进制）+ SCM 服务注册（`PolarisHelper`）。
//!
//! ## 状态模型（移植自 `HelperStatus`，Polaris shared/types）
//!
//! [`HelperStatus`] 覆盖：installed / ready / version / loaded / needsRepair / upgradeable。
//! ready = installed ∧ token 有效 ∧ ping 回 proto ≥ [`MIN_USABLE_PROTO`]（上游 `HelperManager.ts:183`）。
//!
//! ## 移植纪律
//!
//! 1. 路径常量 + plist/unit 生成是跨平台纯逻辑（可测）。
//! 2. launchd/systemd/SCM 操作 trait 抽象（[`SysOps`]），测试 mock。
//! 3. `forbid(unsafe_code)`。

use crate::client::HelperClient;
#[cfg(test)]
use crate::client::{ClientError, Connector};
use crate::token;
use polaris_helper_proto::response::{Pong, ResponseKind};
use polaris_helper_proto::{Platform, Request, Response};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// helper 服务标签（移植自 上游 `LABEL = 'com.polaris.helper'`，HelperManager.ts:31）。
/// Polaris 改名 com.polaris.helper。
pub const SERVICE_LABEL: &str = "com.polaris.helper";

/// 「功能齐全、不报需修复」的最低 protoVersion。
///
/// **不移植** 上游的 `MIN_USABLE = 4`：那个 4 只在 mac 谱系 9 代演进里有意义（v4 起 TUN 齐全）。
/// Polaris 三平台统一 v1（见 `polaris_helper_proto` crate 文档），首发 helper 就带齐全部命令
/// ⇒ 门槛 = [`polaris_helper_proto::proto_version::CURRENT`] 的起点 1。
///
/// ⚠️ 这是**最容易被漏掉的连带**：只把常量从 9/5/1 改成 1、却留着 `>= 4`，会让每一台机器的 helper
/// 都判为 `ready=false / needs_repair=true` —— TUN 直接不可用。`min_usable_not_above_current` 锁死。
pub const MIN_USABLE_PROTO: u32 = 1;

/// 三平台安装路径集合（移植自 Polaris 路径常量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// helper 二进制目标路径（root 拥有）。
    pub binary: PathBuf,
    /// 服务描述符**文件**：mac = plist，linux = systemd unit。
    ///
    /// **Windows 恒 `None`** —— 其服务定义活在 SCM 里，磁盘上没有对应文件。早先这里塞了一个
    /// 从不存在的 `helper-service.yml` 占位路径，而 [`HelperManager::is_installed`] 又拿它做
    /// 存在性判定 ⇒ Windows 上恒判「未安装」。「已安装」的第二条证据改由
    /// [`SysOps::service_exists`] 查 SCM 提供。
    pub descriptor: Option<PathBuf>,
    /// socket / pipe 路径（client 连接目标）。
    pub socket: PathBuf,
    /// 受保护核目录（sing-box 锁定路径）。**win 不用**（核走 app 侧，见 [`InstallParams`]）。
    pub core_dir: PathBuf,
    /// 服务标识：mac = launchd label，linux = systemd unit 名，**win = SCM 服务名**。
    ///
    /// 早先 `is_loaded`/`start`/`stop` 一律硬传 [`SERVICE_LABEL`]（`com.polaris.helper`），
    /// 而 Windows 装出来的服务叫 `PolarisHelper` ⇒ `sc query/start/stop` 全部打空。
    pub service_label: &'static str,
}

/// 三平台安装路径工厂（移植自 Polaris mac 常量，扩展 linux/win）。
impl InstallPaths {
    /// macOS 路径（移植自 `HelperManager.ts:30,33-35`）。
    #[must_use]
    pub fn mac() -> Self {
        Self {
            // ⚠️ **整串 `from` 而不是 `from(dir).join(leaf)`**（2026-08-05，Windows CI 血证）：
            // 这些是要写进 **macOS shell 脚本**的路径字面量，分隔符必须恒为 `/`。而 `join` 用的是
            // **宿主平台**的分隔符 —— 本 crate 三平台都编译，在 Windows 上会产出
            // `/Library/PrivilegedHelperTools\com.polaris.helper` 这种混合形态。
            // 生产只在 mac 跑，所以不是线上缺陷；但它让 Windows 腿的测试恒红，而 Windows 腿恒红
            // ⇒ rust-cache 的 post 步骤（save-if: success()）永不保存 ⇒ 每轮都冷编译 20 分钟。
            binary: PathBuf::from(format!("/Library/PrivilegedHelperTools/{SERVICE_LABEL}")),
            descriptor: Some(PathBuf::from(format!(
                "/Library/LaunchDaemons/{SERVICE_LABEL}.plist"
            ))),
            socket: PathBuf::from("/Library/Application Support/Polaris/helper.sock"),
            core_dir: PathBuf::from("/Library/Application Support/Polaris/core"),
            service_label: SERVICE_LABEL,
        }
    }

    /// Linux 路径（systemd unit + unix socket + SO_PEERCRED）。
    #[must_use]
    pub fn linux() -> Self {
        Self {
            binary: PathBuf::from("/usr/local/lib/polaris/helper"),
            descriptor: Some(PathBuf::from("/etc/systemd/system/polaris-helper.service")),
            socket: PathBuf::from("/run/polaris/helper.sock"),
            core_dir: PathBuf::from("/usr/local/lib/polaris/core"),
            service_label: SERVICE_LABEL,
        }
    }

    /// Windows 路径（SCM 服务 + 命名管道）。
    #[must_use]
    pub fn win() -> Self {
        Self {
            // 必须与 `build_win_install_script` 的落点逐字一致 —— 二者曾各拼各的（状态探测查
            // `C:\Program Files\Polaris\helper.exe`，脚本装到 `C:\ProgramData\Polaris\...`），
            // 分叉导致 Windows 上 `is_installed` 恒 false。现同源于 WIN_SUPPORT_DIR/WIN_HELPER_EXE，
            // 由 `win_install_script_targets_the_same_paths_status_probes` 钉死。
            // 字面量拼接而非 `join`：`PathBuf::join` 用**宿主**分隔符，在 Linux/mac 上构造
            // Windows 路径会得到 `C:\ProgramData\Polaris/polaris-helper.exe` 这种混合形态，
            // 与脚本里的 `format!(r"{support}\{WIN_HELPER_EXE}")` 对不上。
            binary: PathBuf::from(format!(r"{WIN_SUPPORT_DIR}\{WIN_HELPER_EXE}")),
            // Windows 服务定义在 SCM，磁盘无描述符文件 → is_installed 走 SysOps::service_exists
            // 单证据（W17：support 目录 ACL 锁拒未提权 stat，文件证据不可用；取舍见 is_installed 头注）。
            descriptor: None,
            socket: PathBuf::from(r"\\.\pipe\polaris-helper"),
            // win 不播种受管核（核走 app 侧，`InstallParams::bundled_core` 在 win 忽略）；
            // 此字段在 win 无消费者，留占位值仅为结构完整。
            core_dir: PathBuf::from(format!(r"{WIN_SUPPORT_DIR}\core")),
            service_label: WIN_SERVICE_NAME,
        }
    }

    /// 按平台取路径。
    #[must_use]
    pub fn for_platform(platform: Platform) -> Self {
        match platform {
            Platform::Mac => Self::mac(),
            Platform::Linux | Platform::Other => Self::linux(),
            Platform::Win => Self::win(),
        }
    }
}

/// helper 状态快照（移植自 上游 `HelperStatus`，shared/types）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelperStatus {
    /// 是否已安装（二进制 + 描述符存在）。
    pub installed: bool,
    /// 是否就绪（installed + token 有效 + ping proto ≥ MIN_USABLE）。
    pub ready: bool,
    /// 报告的 protoVersion（ping 回）。
    pub version: Option<u32>,
    /// helper 自报构建身份；旧 helper 的 pong 不带该字段。
    pub build_id: Option<String>,
    /// 是否可升级（ready ∧（proto 落后，或同 proto 的构建身份与随包 helper 不一致））。
    pub upgradeable: bool,
    /// 是否需修复（installed 但 !ready，如 token 丢失 / proto 过旧）。
    pub needs_repair: bool,
}

/// helper 生命周期管理器。
///
/// 持有平台 + 安装路径 + connector（ping 探测）+ 系统操作抽象。
/// install/uninstall/start/stop 是**决策 + 委托** —— 真系统操作走 [`SysOps`]。
pub struct HelperManager {
    platform: Platform,
    paths: InstallPaths,
    token_path: PathBuf,
    sysops: Box<dyn SysOps>,
}

/// 系统操作抽象（移植自 Polaris 的 spawn(launchctl) / systemctl / SCM 操作）。
///
/// 把「文件存在检查 / launchd bootstrap / systemctl enable / SCM install / 启停服务」抽成 trait，
/// 让 [`HelperManager`] 的生命周期决策可在 Linux 上测（mock [`SysOps`]）。
pub trait SysOps: Send {
    /// 检查路径是否存在（移植自 上游 `fs.existsSync`，HelperManager.ts:202）。
    fn exists(&self, path: &Path) -> bool;

    /// 启动 helper daemon（mac: launchctl bootstrap；linux: systemctl start；win: sc start）。
    /// 返回成功 / 失败消息。
    fn start_service(&self, label: &str) -> Result<(), String>;

    /// 停止 helper daemon（mac: launchctl bootout；linux: systemctl stop；win: sc stop）。
    fn stop_service(&self, label: &str) -> Result<(), String>;

    /// 是否已加载/**正在运行**（mac: launchctl print；linux: systemctl is-active；win: sc query 且 RUNNING）。
    fn is_loaded(&self, label: &str) -> bool;

    /// 服务是否**已注册**（不要求正在运行）。
    ///
    /// 与 [`SysOps::is_loaded`] 的区别是本方法对「装了但停着」返回 `true` —— 这正是
    /// [`HelperManager::is_installed`] 在 Windows 上需要的语义（SCM 里有服务定义即算装过；
    /// 若沿用 `is_loaded`，一台 helper 停着的机器会被判成从没装过，进而丢掉可修复态）。
    fn service_exists(&self, label: &str) -> bool;
}

impl HelperManager {
    /// 构造管理器。`token_path` = app 侧 token 文件路径（上游 `getUserDataPath()/helper-client.token`）。
    pub fn new(platform: Platform, token_path: PathBuf, sysops: Box<dyn SysOps>) -> Self {
        Self {
            platform,
            paths: InstallPaths::for_platform(platform),
            token_path,
            sysops,
        }
    }

    /// 当前平台。
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// 安装路径集。
    #[must_use]
    pub fn paths(&self) -> &InstallPaths {
        &self.paths
    }

    /// 当前 app 侧 token（从 token 文件读，缺失返回空）。
    #[must_use]
    pub fn token(&self) -> String {
        token::read_token(&self.token_path)
    }

    /// 探测 helper 是否已安装 = **二进制在位 + 服务已注册**。
    ///
    /// 移植自 上游 `filesPresent()`（HelperManager.ts:202-205）：
    /// ```ignore
    /// return fs.existsSync(HELPER_DEST) && fs.existsSync(PLIST_PATH);
    /// ```
    ///
    /// 证据构成按平台（W17 后显式分歧）：
    /// - **Windows：SCM 单证据**（[`SysOps::service_exists`]，`sc query` 退出码，不要求
    ///   RUNNING）。不再 stat 二进制——安装脚本把 support 目录锁成 SYSTEM/Administrators-only，
    ///   UAC 过滤令牌的未提权 app 连 stat 都被拒（2026-08-19 真机对照实测），文件证据恒 false。
    ///   取舍：「拷贝成功 + New-Service 失败」的孤儿二进制态判未装（可重装，脚本 Force 覆盖）；
    ///   「服务在 + 二进制被手删」判已装（ping 挂 → needs_repair 可修复，优于误报未装）。
    /// - **mac/linux：binary + 描述符双证据**（plist/unit 可 stat，目录无 ACL 锁）。
    ///
    /// 共同底线：「装了但没跑」必须仍判已安装，否则 `compute_status_with_client` 短路成
    /// 全 false、连管道都不 ping，把可修复态误报成未安装态。
    #[must_use]
    pub fn is_installed(&self) -> bool {
        // 证据集按 **Platform 枚举**分派（2026-08-20 订正：原先按编译目标 cfg 分叉——win 编译
        // 目标上非 Win 平台的管理器也被拽进 SCM 分支，CI win 腿六测全红；而 push 只跑 ubuntu 腿，
        // 全矩阵 dispatch 才暴露）。生产行为不变：真 Windows 上 Platform 恒为 Win。
        match self.platform {
            // Windows（W17，2026-08-19 首次成功安装后暴露）：安装脚本把 support 目录 ACL 锁成
            // SYSTEM/Administrators-only（token/exe 出生即私有，防窃取），而 Administrators 组在
            // **UAC 过滤令牌**里是 deny-only ⇒ 未提权 app 连 stat 二进制都被拒（真机实测
            // Test-Path=False，提权=True）→ 文件证据恒 false → 状态卡「未安装」。故 **SCM 单证据**：
            // 服务由安装脚本与二进制同事务创建（New-Service 在 Copy-Item 之后），SCM 查询未提权
            // 可用（真机实测 sc query exit=0）。取舍：孤儿二进制（拷了没建成服务）判未装可重装；
            // 服务在而二进制被手删判已装（ping 挂 → needs_repair 可修复，优于误报未装）。
            Platform::Win => self.sysops.service_exists(self.paths.service_label),
            // mac/linux：support 目录无此 ACL 锁（plist/unit 可 stat），binary+描述符双证据。
            _ => {
                if !self.sysops.exists(&self.paths.binary) {
                    return false;
                }
                match &self.paths.descriptor {
                    Some(desc) => self.sysops.exists(desc),
                    None => self.sysops.service_exists(self.paths.service_label),
                }
            }
        }
    }

    /// 是否已加载/运行（launchd/systemd/SCM 视角）。
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.sysops.is_loaded(self.paths.service_label)
    }

    /// 启动 helper daemon（委托 [`SysOps::start_service`]）。
    pub fn start(&self) -> Result<(), String> {
        self.sysops.start_service(self.paths.service_label)
    }

    /// 停止 helper daemon（委托 [`SysOps::stop_service`]）。
    pub fn stop(&self) -> Result<(), String> {
        self.sysops.stop_service(self.paths.service_label)
    }

    /// 安装前的 token 准备：复用已有 token，否则生成并写盘（移植自 `HelperManager.ts:478-485`）。
    ///
    /// 返回 token（已写盘）。这步是纯文件操作（不需要 root），在 root 安装脚本之前由 app 跑。
    pub fn prepare_token(&self) -> Result<String, std::io::Error> {
        let existing = self.token();
        if !existing.is_empty() {
            // 复用已有 token（Polaris install 复用，避免 root 侧 helper.token 与 client token 失同步）
            return Ok(existing);
        }
        token::write_token(&self.token_path)
    }

    /// 卸载后的 token 清理：删 app 侧 token 文件（移植自 `HelperManager.ts:567-571`）。
    pub fn clear_token(&self) {
        token::remove_token(&self.token_path);
    }
}

/// 本 build 期望的 protoVersion（**与平台无关**）。
///
/// 上游 这里按平台分三支（mac=9 / win=5 / linux=1），因为它的三套 Go helper 各自演进。Polaris 三
/// 平台共用一个 `helper-proto` crate、helper 也是同一份 Rust 代码按 target 编译 ⇒ 期望值只有一个。
/// 保留成函数（而非内联常量）是为了留住 `upgradeable` 的语义：装着的 helper 比本 build 期望的旧 →
/// 提示可升级；`CURRENT` 将来 +1 时这条自动生效。
fn expected_proto() -> u32 {
    polaris_helper_proto::proto_version::CURRENT
}

/// ping helper 取 protoVersion + build identity（移植自 上游 `sendCommand(['ping'], 1500)` + 正则提取）。
///
/// 返回 None 当 ping 失败或响应非 Pong。
fn ping_for_handshake(client: &HelperClient) -> Option<Pong> {
    let resp = client
        .send_with_timeout(&Request::Ping, Duration::from_millis(1500))
        .ok()?;
    match resp {
        Response::Ok(ResponseKind::Pong(p)) => Some(p),
        _ => None,
    }
}

impl HelperManager {
    /// 计算完整状态（接收已构造的 HelperClient，避免 Connector 克隆问题）。
    ///
    /// 这是推荐入口 —— 调用方持有 HelperClient，HelperManager 仅做决策。
    pub fn compute_status_with_client(&self, client: &HelperClient) -> HelperStatus {
        let installed = self.is_installed();
        if !installed {
            return HelperStatus::default();
        }
        let tok = self.token();
        if tok.is_empty() {
            return HelperStatus {
                installed: true,
                needs_repair: true,
                ..Default::default()
            };
        }
        let handshake = match ping_for_handshake(client) {
            Some(v) => v,
            None => {
                return HelperStatus {
                    installed: true,
                    needs_repair: true,
                    ..Default::default()
                };
            }
        };
        let proto_version = handshake.proto_version;
        let ready = proto_version >= MIN_USABLE_PROTO;
        let expected_proto = expected_proto();
        let expected_build_id = polaris_helper_proto::build_identity::current();
        let same_proto_build_mismatch = proto_version == expected_proto
            && handshake.build_identity.as_deref() != Some(expected_build_id);
        let upgradeable = ready && (proto_version < expected_proto || same_proto_build_mismatch);
        HelperStatus {
            installed: true,
            ready,
            version: Some(proto_version),
            build_id: handshake.build_identity,
            upgradeable,
            needs_repair: !ready,
        }
    }

    /// W20（2026-08-20）：带恢复腿的状态探测——「装了但停着」不再直接判 needs_repair，先拉起复核。
    ///
    /// 背景：Windows 手动结束 helper 进程（或服务停着）时，[`compute_status_with_client`](Self::compute_status_with_client)
    /// 的 ping 挂 → needs_repair → UI 引导「修复助手」；但此时结构完好，把服务拉起来即可恢复。
    /// 三平台自愈对齐：mac=plist `KeepAlive`、linux=unit `Restart=on-failure`、win=安装脚本配
    /// SCM 失败恢复（`sc failure ... restart/5000`，覆盖异常终止）+ 本腿（覆盖干净 stop / 恢复窗
    /// 耗尽等残余态）。win 端未提权拉起的可行性由安装脚本 `sdset` 授 IU `SERVICE_START` 保证
    /// （默认 DACL 无此权，.207 实测 IU 只有查询权）。
    ///
    /// 分型（哪些 needs_repair 值得试拉起；拉错场景要么白拉、要么误动服务）：
    /// - token 为空：拉起也过不了鉴权，与「停着」无关 → 维持修复流；
    /// - 服务正在跑（is_loaded）仍 ping 不通：结构性问题（proto 过旧 / 管道损坏）→ 维持修复流；
    /// - 服务停着：拉起（start）→ 复用 [`wait_until_ready`](Self::wait_until_ready) 轮询复核。
    ///   拉不起（如二进制被删）或复核仍不 ready → 维持 needs_repair（真坏该修，不粉饰）。
    ///
    /// 每次调用至多拉一次。接线点：`HelperRuntime::status`（设置页 / 启动 7s 探测 / 重新检测共用）；
    /// 仅停着态付出 start + 轮询的耗时（轮询窗 5s，管道未绑时连接即刻失败不占 ping 超时，挂起连接
    /// 最坏 1.5s/次），正常路径零额外成本——故 status 消费方须在非主线程跑（见 helper_get_status）。
    #[must_use]
    pub fn status_with_recovery(&self, client: &HelperClient) -> HelperStatus {
        self.status_with_recovery_poll(client, RECOVERY_POLL_ATTEMPTS, RECOVERY_POLL_DELAY)
    }

    /// [`status_with_recovery`](Self::status_with_recovery) 的参数化形态（轮询次数/间隔透传，
    /// 与 [`wait_until_ready`](Self::wait_until_ready) 同一套测试缝隙）。
    #[must_use]
    pub fn status_with_recovery_poll(
        &self,
        client: &HelperClient,
        attempts: u32,
        delay: Duration,
    ) -> HelperStatus {
        let status = self.compute_status_with_client(client);
        if !status.needs_repair {
            return status;
        }
        // token 缺失的 needs_repair 不是「停着」能解释的（拉起也过不了鉴权）。
        if self.token().is_empty() {
            return status;
        }
        // 跑着仍 ping 不通 → 结构性问题，交回修复流。
        if self.is_loaded() {
            return status;
        }
        if let Err(e) = self.start() {
            log::warn!("helper 恢复腿：拉起停着的服务失败（{e}）→ 维持 needs_repair");
            return status;
        }
        self.wait_until_ready(client, attempts, delay)
    }
}

// ============================================================================
// 装卸全流程（install / uninstall）—— 移植自 Polaris 三平台 install 编排：
//   mac: HelperManager.ts:buildInstallScript/buildUninstallScript/runRootScript
//   linux: LinuxServiceHelper.ts:buildInstallScript/buildUnit/runPkexecScript
//   win: WindowsServiceHelper.ts:buildInstallScript/buildUninstallScript/runElevatedPowerShell
//
// **单一编排真值源**（编排者拍板）：install 是「生成一个 root 脚本（内含 拷二进制 + 写 root 侧 token +
// 播种核 + 写 plist/unit/SCM 描述 + bootstrap）→ 经提权（osascript/pkexec/UAC）跑一次」。plist/unit/SCM
// 描述**内嵌进脚本**（root 侧写），与 上游 逐字对齐。launchd.rs 降为纯 render（不再自持 bootstrap 编排）。
// ============================================================================

use crate::privilege::{
    osascript_escalation, pkexec_escalation, run_escalation, shell_quote, uac_escalation,
    Escalation, EscalationOutcome, Executor,
};
use std::io;

/// mac `--support` 目录（= daemon `daemon.rs:DEFAULT_SUPPORT_DIR`，helper.token/socket/core 落此）。
const MAC_SUPPORT_DIR: &str = "/Library/Application Support/Polaris";
/// linux systemd 服务名（= `InstallPaths::linux().descriptor` 文件名）。
const LINUX_SERVICE_NAME: &str = "polaris-helper.service";
/// linux 授权 uid 列表（= daemon `--authfile` 默认，`server.rs:DEFAULT_AUTH_FILE`）。
const LINUX_AUTH_FILE: &str = "/var/lib/polaris/authorized-uids";
/// linux 授权文件所在状态目录（`AUTH_FILE` 的父）。
const LINUX_STATE_DIR: &str = "/var/lib/polaris";
/// win SCM 服务名（= daemon `windows/mod.rs:SERVICE_NAME`）。
const WIN_SERVICE_NAME: &str = "PolarisHelper";
/// win support 目录（= daemon `--support` 默认，`windows/mod.rs:DEFAULT_SUPPORT_DIR`；helper.exe 外置副本 + helper.token 落此）。
const WIN_SUPPORT_DIR: &str = r"C:\ProgramData\Polaris";
/// win helper 外置副本文件名。**单一真相源**：[`InstallPaths::win`] 与
/// [`build_win_install_script`] 必须同取此常量。早先脚本用 `win_basename(src_binary)` 现算、
/// 而 `InstallPaths::win()` 另写死一个不同路径，两者分叉即 Windows 恒判「未安装」的成因。
const WIN_HELPER_EXE: &str = "polaris-helper.exe";

/// install 就绪轮询：装完等 daemon 起来绑 socket/pipe 的次数。2026-08-19 真机实测从上游的
/// `for i<10` 放宽：脚本侧 sc 删旧等待窗（≤15s）+ New-Service 重试后，服务起管道常超 3s，
/// 旧窗口把响应快照定格在「未就绪」，卡片停在安装前旧态（W10 跟进项，.207 首装实测）。
pub const READY_POLL_ATTEMPTS: u32 = 20;
/// install 就绪轮询间隔（同上从 300ms 放宽：总窗 10s，覆盖 SCM 冷启动 + 管道监听）。
pub const READY_POLL_DELAY: Duration = Duration::from_millis(500);

/// W20 恢复腿轮询次数：拉起已装服务后等管道绑定。较 install 的 20 次减半——恢复拉的是已装好的
/// 服务（无脚本侧删旧/拷贝/建服务竞态），要等的只有「服务起 → 管道监听」这一段（真机实测可 >3s）。
const RECOVERY_POLL_ATTEMPTS: u32 = 10;
/// W20 恢复腿轮询间隔（总窗 5s，依据同上）。
const RECOVERY_POLL_DELAY: Duration = Duration::from_millis(500);

/// 装卸流程错误（escalation 之前的失败：二进制缺失 / token 写失败 / 脚本落盘失败 / 平台不支持）。
///
/// escalation 本身的三态（成功/取消/失败）由 [`EscalationOutcome`] 表达（非 `Err`）—— 用户取消是正常流程，不是错误。
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// helper 源二进制缺失（构建未包含），对齐 上游 `!fs.existsSync(srcBinary)`。
    #[error("helper 二进制缺失: {0}")]
    HelperBinaryMissing(PathBuf),
    /// app 侧 token 写盘失败（移植 Polaris install 的 `写入 token 失败`）。
    #[error("写入 token 失败: {0}")]
    TokenWrite(io::Error),
    /// 安装脚本落盘失败（移植 Polaris runRootScript/runPkexecScript 的 catch）。
    #[error("写入安装脚本失败: {0}")]
    ScriptWrite(io::Error),
    /// 提权执行本身失败（spawn 失败等，非「用户取消」——取消是 [`EscalationOutcome::Cancelled`]）。
    #[error("提权执行失败: {0}")]
    Escalation(#[from] crate::client::ClientError),
    /// 本编译目标不支持该平台的装卸（如 Linux 二进制上构造 `Platform::Mac` 的装脚本 —— 运行期不会发生）。
    #[error("本构建目标不支持平台 {0:?} 的装卸")]
    UnsupportedPlatform(Platform),
}

/// 安装参数（路径由调用方解析 —— HelperManager 不知道 Tauri 资源布局，对齐「决策+委托」）。
///
/// 各平台按需取用，无关字段忽略（移植锚点见各字段）。
#[derive(Debug, Clone)]
pub struct InstallParams {
    /// 源 helper 二进制（app 资源内 `polaris-helper`，脚本拷到特权路径）。
    /// 上游: `resourceManager.getMacHelperPath()` / `getLinuxHelperPath()` / `getWinHelperPath()`。
    pub src_binary: PathBuf,
    /// 随包 sing-box 核（mac/linux 播种 root 受管核；**win 忽略**——win 核走 app 侧）。
    /// 上游: `resourceManager.getBundledSingBoxPath()`。
    pub bundled_core: PathBuf,
    /// win `--singbox` 指向的 sing-box 路径（app 侧核）。**mac/linux 忽略**（用锁定的 `core_dir/sing-box`）。
    /// 上游(win): `resourceManager.getSingBoxPath()`。
    pub singbox_path: PathBuf,
    /// 用户 config/data 目录（`--confdir`；**linux 忽略**——核以登录用户跑，config 属主天然对）。
    /// 上游: `getUserDataPath()`。
    pub conf_dir: PathBuf,
    /// 授权 uid（linux 写 `authorized-uids`；**mac/win 忽略**）。上游(linux): `process.getuid()`。
    pub uid: u32,
    /// 安装脚本临时落盘目录（安全 0700 私有目录；调用方传 `userData/priv` 等）。
    /// 上游: `getUserDataPath()/priv`(mac) / `getUserDataPath()`(linux) / `os.tmpdir()`(win)。
    pub script_dir: PathBuf,
}

impl HelperManager {
    /// 安装/修复 helper（全流程，移植自三平台 `install()`）。
    ///
    /// 步骤：① 校验源二进制存在 → ② 准备 app 侧 token（复用或生成，[`prepare_token`](Self::prepare_token)）
    /// → ③ 生成 root 安装脚本（内嵌 拷二进制 + 写 root token + 播种核 + 写 plist/unit/SCM 描述 + bootstrap）
    /// → ④ 脚本安全落盘（随机名 + 0700 + O_EXCL）经 [`Executor`] 提权跑（osascript/pkexec/UAC）→ ⑤ 清理脚本。
    ///
    /// 返回 [`EscalationOutcome`]（成功/用户取消/脚本失败）。**就绪等待**由调用方在成功后用
    /// [`wait_until_ready`](Self::wait_until_ready) 轮询（需 [`HelperClient`]，与提权解耦、各自可测）。
    pub fn install(
        &self,
        params: &InstallParams,
        executor: &dyn Executor,
    ) -> Result<EscalationOutcome, ManagerError> {
        // ① 源二进制存在（Polaris: !fs.existsSync(srcBinary) → 缺失）。
        if !self.sysops.exists(&params.src_binary) {
            return Err(ManagerError::HelperBinaryMissing(params.src_binary.clone()));
        }
        // ② token：复用或生成并写 app 侧（linux 无 token 语义，写了也无害；脚本不会用）。
        let token = self.prepare_token().map_err(ManagerError::TokenWrite)?;
        // ③ 生成 root 安装脚本。
        let script = self.build_install_script(params, &token)?;
        // ④⑤ 落盘 + 提权 + 清理。
        self.run_privileged_script(params, self.install_script_name(), &script, executor)
    }

    /// 卸载 helper（全流程，移植自三平台 `uninstall()` 的提权路径）。
    ///
    /// 生成 root 卸载脚本（bootout/disable + 删描述符/二进制/受保护目录）→ 提权跑 → 清 app 侧 token。
    /// 返回 [`EscalationOutcome`]。
    ///
    /// **win 零 UAC 优化**（管道自卸载）见 [`pipe_self_uninstall`](Self::pipe_self_uninstall) ——
    /// 调用方可先试它、失败再回退本方法（对齐 WindowsServiceHelper.ts:361 的「先管道后提权」）。
    pub fn uninstall(
        &self,
        script_dir: &Path,
        executor: &dyn Executor,
    ) -> Result<EscalationOutcome, ManagerError> {
        let script = self.build_uninstall_script();
        // uninstall 脚本不依赖 InstallParams 的其它字段，仅需 script_dir 落盘。
        let params = InstallParams {
            src_binary: PathBuf::new(),
            bundled_core: PathBuf::new(),
            singbox_path: PathBuf::new(),
            conf_dir: PathBuf::new(),
            uid: 0,
            script_dir: script_dir.to_path_buf(),
        };
        let outcome =
            self.run_privileged_script(&params, self.uninstall_script_name(), &script, executor)?;
        // 卸载成功/取消都清 app 侧 token（重装会重生成；取消时清了也无害——未装态 token 无意义）。
        if matches!(outcome, EscalationOutcome::Success) {
            self.clear_token();
        }
        Ok(outcome)
    }

    /// win 零 UAC 管道自卸载（移植 WindowsServiceHelper.ts:371-390）：经就绪 helper 发 `uninstall`，
    /// helper 以 SYSTEM 收割 child + 派生旁路自停删服务 + 删 ProgramData（见 daemon W11/W12）。
    ///
    /// 返回 `true` = helper 回 `OK`（自卸载已启动，调用方随后轮询服务消失）；`false` = 管道不可用/非 OK
    /// （调用方回退 [`uninstall`](Self::uninstall) 提权兜底）。仅 win 有 `uninstall` 命令，其余平台恒 `false`。
    #[must_use]
    pub fn pipe_self_uninstall(&self, client: &HelperClient) -> bool {
        if self.platform != Platform::Win {
            return false;
        }
        matches!(client.send(&Request::Uninstall), Ok(Response::Ok(_)))
    }

    /// 装完轮询就绪（移植 Polaris install 尾 `for i<10 && !ready { sleep(300); recompute }`）。
    ///
    /// 每轮 [`compute_status_with_client`](Self::compute_status_with_client)（内含 ping）。`client` 须用
    /// 生产 [`Connector`](crate::client::Connector)（每 ping 重连）。返回最终状态（ready 或轮询耗尽）。
    #[must_use]
    pub fn wait_until_ready(
        &self,
        client: &HelperClient,
        attempts: u32,
        delay: Duration,
    ) -> HelperStatus {
        let mut status = self.compute_status_with_client(client);
        for _ in 0..attempts {
            if status.ready {
                break;
            }
            std::thread::sleep(delay);
            status = self.compute_status_with_client(client);
        }
        status
    }

    // ── 脚本生成分派 ────────────────────────────────────────────────────────
    /// 生成 root 安装脚本（按平台分派）。
    fn build_install_script(
        &self,
        params: &InstallParams,
        token: &str,
    ) -> Result<String, ManagerError> {
        match self.platform {
            Platform::Mac => Ok(build_mac_install_script(&self.paths, params, token)),
            Platform::Linux | Platform::Other => {
                Ok(build_linux_install_script(&self.paths, params))
            }
            Platform::Win => Ok(build_win_install_script(params, token)),
        }
    }

    /// 生成 root 卸载脚本（按平台分派）。
    fn build_uninstall_script(&self) -> String {
        match self.platform {
            Platform::Mac => build_mac_uninstall_script(&self.paths),
            Platform::Linux | Platform::Other => build_linux_uninstall_script(&self.paths),
            Platform::Win => build_win_uninstall_script(),
        }
    }

    /// 安装脚本文件名（含平台正确扩展名：mac/linux=`.sh` bash、win=`.ps1` PowerShell）。
    const fn install_script_name(&self) -> &'static str {
        match self.platform {
            Platform::Win => "polaris-helper-install.ps1",
            _ => "polaris-helper-install.sh",
        }
    }

    const fn uninstall_script_name(&self) -> &'static str {
        match self.platform {
            Platform::Win => "polaris-helper-uninstall.ps1",
            _ => "polaris-helper-uninstall.sh",
        }
    }

    // ── 提权跑脚本（落盘 + escalation + 清理，移植 runRootScript/runPkexecScript/runElevatedPowerShell）──
    /// 脚本安全落盘（随机名 + 0700 + O_EXCL）→ 构造提权 [`Escalation`] → [`run_escalation`] → 清理脚本。
    fn run_privileged_script(
        &self,
        params: &InstallParams,
        name: &str,
        script: &str,
        executor: &dyn Executor,
    ) -> Result<EscalationOutcome, ManagerError> {
        // TOCTOU 加固（移植 HelperManager.ts:872-882）：私有 0700 目录 + 随机名 + O_EXCL 独占创建。
        let script_path = write_secure_script(&params.script_dir, name, script)
            .map_err(ManagerError::ScriptWrite)?;
        let script_path_str = script_path.to_string_lossy().into_owned();
        let escalation = self.build_escalation(&script_path_str);
        let outcome = run_escalation(&escalation, executor);
        // finally: 清理脚本（对齐 Polaris runRootScript 的 unlink，成功失败都删）。
        let _ = std::fs::remove_file(&script_path);
        Ok(outcome?)
    }

    /// 按平台构造提权决策（复用 [`privilege`](crate::privilege) 的真逻辑 argv 构造）。
    fn build_escalation(&self, script_path: &str) -> Escalation {
        match self.platform {
            Platform::Mac => osascript_escalation(script_path),
            Platform::Linux | Platform::Other => pkexec_escalation(script_path),
            Platform::Win => uac_escalation(script_path),
        }
    }
}

// ===== 脚本安全落盘（TOCTOU 加固）=====

/// 生成随机后缀（不可预测的脚本文件名，防同用户进程抢占落点 —— 对齐 上游 `randomBytes(12)`）。
///
/// 用 OS 熵源（/dev/urandom），失败回退时间+计数（脚本落私有 0700 目录 + O_EXCL 已是主防线，随机名是纵深）。
fn random_suffix() -> String {
    let mut bytes = [0u8; 12];
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .is_err()
        {
            fill_fallback(&mut bytes);
        }
    }
    #[cfg(not(unix))]
    {
        fill_fallback(&mut bytes);
    }
    let mut s = String::with_capacity(24);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn fill_fallback(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
    for (i, byte) in buf.iter_mut().enumerate() {
        let mix = now
            .wrapping_add(pid)
            .wrapping_add(cnt)
            .wrapping_add(i as u64);
        *byte = mix.to_le_bytes()[i % 8];
    }
}

/// 脚本落盘到 `dir/<random>-<name>`：私有 0700 目录 + O_EXCL 独占创建 + 0700 文件（unix）。
///
/// 移植 HelperManager.ts:872-882 的 TOCTOU 加固（`mkdirSync(mode:0700)` + `writeFileSync(mode:0700, flag:'wx')`）。
/// 提权脚本的落盘字节：Windows 前置 UTF-8 BOM，unix 原样。
///
/// **参数化而非 `cfg!`**：BOM 这件事的正确性只在 Windows 上有后果，而 Windows 分支在 Linux 上
/// 根本不编译 —— 用 `#[cfg]` 写就等于「本机永远测不到、CI 交叉 check 也只能看编不编得过」。
/// 收成参数后两个分支在 Linux 上都跑得到，判据落在纯逻辑上（同 `system-integration` 那条
/// 「命令构造与输出解析全是纯函数」的纪律）。
fn script_bytes(content: &str, windows: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 3);
    if windows {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out.extend_from_slice(content.as_bytes());
    out
}

fn write_secure_script(dir: &Path, name: &str, content: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        // 私有目录收紧到 0700（best-effort：已 0700 或无权改则忽略，对齐 Polaris try/catch chmod）。
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    let path = dir.join(format!("{}-{name}", random_suffix()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // O_EXCL（create_new）拒绝已存在文件/符号链接抢占；0700。
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)?;
        // unix 侧**绝不能**加 BOM：`#!/bin/bash` 前面多三个字节，shebang 就不再是首字节。
        f.write_all(&script_bytes(content, false))?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        // win：os.tmpdir 落 .ps1；create_new 拒绝抢占（win 无 mode）。
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        // 🔴 **必须带 UTF-8 BOM**：`uac_escalation` 用的是系统自带的 `powershell.exe`（= Windows
        // PowerShell 5.1），它对**无 BOM** 的 .ps1 按系统 **ANSI 代码页**解码，不是 UTF-8。
        //
        // 后果不是「注释乱码」这种观感问题：脚本正文里的 `--confdir "<app config dir>"` 含用户 profile
        // 路径，中文账户（`C:\Users\张三\...`）的 UTF-8 字节被按 CP936 解出另一串汉字 ⇒ 服务
        // `BinaryPathName` 指向不存在的目录 ⇒ 之后每次起核都被 helper 的 `cfg_allowed` 判 denied，
        // 而安装本身「成功」。NSIS 安装态下 app 本体也在 `%LOCALAPPDATA%` ⇒ `$helperSrc` 同样中招，
        // 会更早死在 `Copy-Item`。
        //
        // 同仓姊妹腿早就踩过并修好了，只是没推广到这一条：`src-tauri/src/runtime/update_install.rs`
        // 的 `utf16le_with_bom` —— 那里的文档逐字写着 `wscript.exe` 按系统代码页解释无 BOM 脚本、
        // 中文用户名路径会让 `fso.CopyFile` 找不到文件。同一根因，两条腿只修了一条。
        //
        // 选 UTF-8 BOM 而非 UTF-16LE：内容本来就是 Rust 的 UTF-8 `str`，加三字节前缀即可，
        // 无需转码；PowerShell 5.1 与 7 都认这个 BOM。
        f.write_all(&script_bytes(content, true))?;
    }
    Ok(path)
}

// ===== 转义工具 =====

/// XML 字符转义（plist label/路径含 `<`/`&`/`"` 时防破坏 plist，移植 HelperManager.ts:744-750）。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// PowerShell 单引号字符串转义（`''` 转义单引号，移植 WindowsServiceHelper.ts:417）。
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

// `win_basename`（Windows 语义 basename）已随「安装落点单一真相源」收口删除：安装目标文件名
// 不再从 `src_binary` 现算，而是与 `InstallPaths::win()` 同取 `WIN_HELPER_EXE`。

// ===== mac 脚本（移植 HelperManager.ts:772-852）=====

/// mac helper daemon plist 渲染（`Label`/`ProgramArguments`/`KeepAlive`/`RunAtLoad`）。
///
/// **本 crate 侧的 plist 单一 render**（与 daemon 侧 `launchd::render_plist` 语义等价 —— 见报告 DESIGN-REVIEW：
/// launchd.rs 的 render 在 `cfg(any(target_os="macos",test))` 模块内，作为依赖编入时非 mac target 不可见，
/// 故无法跨 crate 复用于 Linux 宿主可测的 install 路径；两处 render 须保持等价直至后续收敛）。
///
/// `program` = HELPER_DEST（daemon 二进制特权路径）；argv = `--singbox <core/sing-box> --confdir <conf>
/// --support <support> --coredir <core>`（对齐 daemon `macos/daemon.rs:parse_args` flag 名）。
fn render_mac_plist(program: &str, core_dir: &str, conf_dir: &str, support_dir: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n\
    <string>{program}</string>\n\
    <string>--singbox</string><string>{singbox}</string>\n\
    <string>--confdir</string><string>{confdir}</string>\n\
    <string>--support</string><string>{support}</string>\n\
    <string>--coredir</string><string>{coredir}</string>\n\
  </array>\n\
  <key>KeepAlive</key><true/>\n\
  <key>RunAtLoad</key><true/>\n\
</dict>\n\
</plist>",
        label = xml_escape(SERVICE_LABEL),
        program = xml_escape(program),
        singbox = xml_escape(&format!("{core_dir}/sing-box")),
        confdir = xml_escape(conf_dir),
        support = xml_escape(support_dir),
        coredir = xml_escape(core_dir),
    )
}

/// mac 安装脚本（bash，root 侧经 osascript 提权跑）。忠实迁自 HelperManager.ts:798-841。
fn build_mac_install_script(paths: &InstallPaths, params: &InstallParams, token: &str) -> String {
    let helper_dest = paths.binary.to_string_lossy();
    // mac 恒有 plist（`InstallPaths::mac()` 建的），只有 Windows 是 None。
    let plist_path = paths
        .descriptor
        .as_ref()
        .expect("mac 必有 plist 描述符")
        .to_string_lossy();
    let core_dir = paths.core_dir.to_string_lossy();
    let support = MAC_SUPPORT_DIR;
    let conf_dir = params.conf_dir.to_string_lossy();
    let src = params.src_binary.to_string_lossy();
    let bundled_sb = params.bundled_core.to_string_lossy();

    let plist = render_mac_plist(&helper_dest, &core_dir, &conf_dir, support);

    format!(
        "#!/bin/bash\n\
set -e\n\
umask 077\n\
SRC={src}\n\
DEST={dest}\n\
SUPPORT={support}\n\
PLIST={plist_path}\n\
mkdir -p /Library/PrivilegedHelperTools \"$SUPPORT\"\n\
# umask 077 会把新建目录设成 700 → 普通用户 app 无法穿越连 socket(EACCES)。目录须 755 可穿越\n\
# （socket 内部仍靠 token 鉴权 + token 文件 600 保护）。\n\
chmod 755 /Library/PrivilegedHelperTools \"$SUPPORT\"\n\
cp \"$SRC\" \"$DEST\"\n\
chown root:wheel \"$DEST\"; chmod 755 \"$DEST\"\n\
printf '%s' {token} > \"$SUPPORT/helper.token\"\n\
chown root:wheel \"$SUPPORT/helper.token\"; chmod 600 \"$SUPPORT/helper.token\"\n\
COREDIR={core_dir}\n\
BUNDLED_SB={bundled_sb}\n\
mkdir -p \"$COREDIR\"\n\
chown root:wheel \"$COREDIR\"; chmod 755 \"$COREDIR\"\n\
if [ ! -x \"$COREDIR/sing-box\" ]; then\n\
  cp \"$BUNDLED_SB\" \"$COREDIR/sing-box.seed.new\"\n\
  mv -f \"$COREDIR/sing-box.seed.new\" \"$COREDIR/sing-box\"\n\
  chown root:wheel \"$COREDIR/sing-box\"; chmod 755 \"$COREDIR/sing-box\"\n\
  xattr -cr \"$COREDIR\" 2>/dev/null || true\n\
  codesign --force --sign - \"$COREDIR/sing-box\" 2>/dev/null || true\n\
fi\n\
cat > \"$PLIST\" <<'POLARIS_PLIST_EOF'\n\
{plist}\n\
POLARIS_PLIST_EOF\n\
chown root:wheel \"$PLIST\"; chmod 644 \"$PLIST\"\n\
launchctl bootout system \"$PLIST\" 2>/dev/null || true\n\
launchctl enable system/{label} 2>/dev/null || true\n\
launchctl bootstrap system \"$PLIST\"\n\
echo installed-ok\n",
        src = shell_quote(&src),
        dest = shell_quote(&helper_dest),
        support = shell_quote(support),
        plist_path = shell_quote(&plist_path),
        token = shell_quote(token),
        core_dir = shell_quote(&core_dir),
        bundled_sb = shell_quote(&bundled_sb),
        plist = plist,
        label = SERVICE_LABEL,
    )
}

/// mac 卸载脚本（bash）。忠实迁自 HelperManager.ts:844-852。
fn build_mac_uninstall_script(paths: &InstallPaths) -> String {
    let plist_path = paths
        .descriptor
        .as_ref()
        .expect("mac 必有 plist 描述符")
        .to_string_lossy();
    let helper_dest = paths.binary.to_string_lossy();
    format!(
        "#!/bin/bash\n\
PLIST={plist_path}\n\
launchctl bootout system \"$PLIST\" 2>/dev/null || true\n\
rm -f \"$PLIST\" {dest}\n\
rm -rf {support}\n\
echo uninstalled-ok\n",
        plist_path = shell_quote(&plist_path),
        dest = shell_quote(&helper_dest),
        support = shell_quote(MAC_SUPPORT_DIR),
    )
}

// ===== linux 脚本（移植 LinuxServiceHelper.ts:308-370）=====

/// linux systemd unit（移植 LinuxServiceHelper.ts:308-328）。
///
/// helper 以 root 跑（无 `User=`）：需 root 才能 setuid 拉 child + 穿越登录用户 userData 校验/重定向；
/// child 的 CAP_NET_ADMIN 由 helper 代码经 AmbientCaps 赋予（不在 unit 层）。`ExecStart` flag 对齐
/// daemon `linux/daemon.rs:parse_args`（`--socket`/`--authfile`/`--coredir`）。
fn build_linux_unit(paths: &InstallPaths) -> String {
    let helper_dest = paths.binary.to_string_lossy();
    let socket = paths.socket.to_string_lossy();
    let core_dir = paths.core_dir.to_string_lossy();
    // RuntimeDirectory 名 = socket 父目录末段（/run/polaris → polaris）。
    let runtime_name = paths
        .socket
        .parent()
        .and_then(|p| p.file_name())
        .map_or_else(
            || "polaris".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
    format!(
        "[Unit]\n\
Description=Polaris privileged network helper\n\
Documentation=https://github.com/Sway-Chan/polaris\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={helper_dest} --socket={socket} --authfile={authfile} --coredir={core_dir}\n\
RuntimeDirectory={runtime_name}\n\
RuntimeDirectoryMode=0755\n\
Restart=on-failure\n\
RestartSec=2\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        helper_dest = helper_dest,
        socket = socket,
        authfile = LINUX_AUTH_FILE,
        core_dir = core_dir,
        runtime_name = runtime_name,
    )
}

/// linux 安装脚本（sh/bash，经 pkexec 提权跑）。忠实迁自 LinuxServiceHelper.ts:330-357。
fn build_linux_install_script(paths: &InstallPaths, params: &InstallParams) -> String {
    let helper_dest = paths.binary.to_string_lossy();
    let core_dir = paths.core_dir.to_string_lossy();
    let core_bin = format!("{core_dir}/sing-box");
    let unit_path = paths
        .descriptor
        .as_ref()
        .expect("linux 必有 systemd unit 描述符")
        .to_string_lossy();
    let src = params.src_binary.to_string_lossy();
    let bundled_core = params.bundled_core.to_string_lossy();
    // libcronet.so 若随包（naive 出站需）随核一并播种。
    let bundled_cronet = params.bundled_core.parent().map_or_else(
        || "libcronet.so".to_owned(),
        |d| d.join("libcronet.so").to_string_lossy().into_owned(),
    );
    let unit = build_linux_unit(paths);

    format!(
        "#!/bin/sh\n\
set -e\n\
install -D -o root -g root -m 0755 {src} {dest}\n\
mkdir -p {core_dir}\n\
chown root:root {core_dir}\n\
chmod 0755 {core_dir}\n\
if [ ! -x {core_bin} ]; then\n\
  install -o root -g root -m 0755 {bundled_core} {core_bin}\n\
  [ -f {bundled_cronet} ] && install -o root -g root -m 0755 {bundled_cronet} {core_dir_cronet} || true\n\
fi\n\
mkdir -p {state_dir}\n\
chmod 0755 {state_dir}\n\
touch {authfile}\n\
chmod 0644 {authfile}\n\
grep -qxF '{uid}' {authfile} || printf '%s\\n' '{uid}' >> {authfile}\n\
cat > {unit_path} <<'POLARIS_UNIT_EOF'\n\
{unit}POLARIS_UNIT_EOF\n\
chmod 0644 {unit_path}\n\
systemctl daemon-reload\n\
systemctl enable --now {service}\n\
echo polaris-helper-install-ok\n",
        src = shell_quote(&src),
        dest = shell_quote(&helper_dest),
        core_dir = shell_quote(&core_dir),
        core_bin = shell_quote(&core_bin),
        bundled_core = shell_quote(&bundled_core),
        bundled_cronet = shell_quote(&bundled_cronet),
        core_dir_cronet = shell_quote(&format!("{core_dir}/libcronet.so")),
        state_dir = shell_quote(LINUX_STATE_DIR),
        authfile = shell_quote(LINUX_AUTH_FILE),
        uid = params.uid,
        unit_path = shell_quote(&unit_path),
        unit = unit,
        service = LINUX_SERVICE_NAME,
    )
}

/// linux 卸载脚本（sh）。忠实迁自 LinuxServiceHelper.ts:360-370。
fn build_linux_uninstall_script(paths: &InstallPaths) -> String {
    let unit_path = paths
        .descriptor
        .as_ref()
        .expect("linux 必有 systemd unit 描述符")
        .to_string_lossy();
    // INSTALL_DIR = helper 二进制父目录（/usr/local/lib/polaris）；RUNTIME_DIR = socket 父（/run/polaris）。
    let install_dir = paths
        .binary
        .parent()
        .map_or_else(|| paths.binary.clone(), Path::to_path_buf);
    let runtime_dir = paths
        .socket
        .parent()
        .map_or_else(|| paths.socket.clone(), Path::to_path_buf);
    format!(
        "#!/bin/sh\n\
systemctl disable --now {service} 2>/dev/null || true\n\
rm -f {unit_path}\n\
rm -rf {install_dir} {state_dir} {runtime_dir}\n\
systemctl daemon-reload 2>/dev/null || true\n\
echo polaris-helper-uninstall-ok\n",
        service = LINUX_SERVICE_NAME,
        unit_path = shell_quote(&unit_path),
        install_dir = shell_quote(&install_dir.to_string_lossy()),
        state_dir = shell_quote(LINUX_STATE_DIR),
        runtime_dir = shell_quote(&runtime_dir.to_string_lossy()),
    )
}

// ===== win 脚本（移植 WindowsServiceHelper.ts:429-517）=====

/// win 安装脚本（PowerShell，经 UAC 提权跑）。忠实迁自 WindowsServiceHelper.ts:429-506。
///
/// 关键步骤：外置 helper.exe 到 `ProgramData\Polaris`（与 app 生命周期解耦）；锁目录/文件 ACL
/// （SYSTEM/Admin 私有）；升级前快照旧服务并备份 helper/token；幂等停删与重建。任一步失败时恢复
/// 旧文件、旧 binPath/start mode 与原运行态，避免覆盖升级把可用 helper 留成半安装态。
/// `$ErrorActionPreference = Stop` 让失败以非零退出透出（提权 executor 归类 Failed；privilege.rs 的
/// uac_escalation 无 上游的 flag-file 错误回写协议 —— 见报告 DESIGN-REVIEW）。
fn build_win_install_script(params: &InstallParams, token: &str) -> String {
    let support = WIN_SUPPORT_DIR;
    let exe = params.src_binary.to_string_lossy();
    // helperDst = SUPPORT\WIN_HELPER_EXE —— **不从 src_binary 现算 basename**：落点必须与
    // `InstallPaths::win().binary` 同源，否则状态探测查一个地方、脚本装到另一个地方（这正是
    // Windows 上 `is_installed` 曾恒 false 的成因）。接线由
    // `win_install_script_targets_the_same_paths_status_probes` 钉死。
    // 单一真相源：与 `InstallPaths::win().binary` 同取 WIN_HELPER_EXE（此前这里用
    // `win_basename(src_binary)` 现算，与状态探测那侧的写死路径分叉 → Windows 恒判未安装）。
    let helper_dst = format!(r"{support}\{WIN_HELPER_EXE}");
    let token_file = format!(r"{support}\helper.token");
    let helper_backup = format!(r"{support}\{WIN_HELPER_EXE}.rollback");
    let token_backup = format!(r"{support}\helper.token.rollback");
    let singbox = params.singbox_path.to_string_lossy();
    let conf_dir = params.conf_dir.to_string_lossy();
    // BinaryPathName：各含空格路径用真双引号包裹，经 New-Service 单一字符串直达 CreateService。
    let bin_path = format!(
        "\"{helper_dst}\" --singbox \"{singbox}\" --confdir \"{conf_dir}\" --support \"{support}\""
    );
    // 🔴 $env: 引用必须走「双引号变量赋值 + 裸变量调用」：PowerShell 单引号是字面量，
    // `& '$env:SystemRoot\icacls.exe'` 不展开 → CommandNotFound + EAP=Stop → 脚本死在
    // New-Item 之后第一条 icacls（目录建了、之后全没做）。2026-08-19 .207 提权重放首曝，
    // 该病自 TS 移植起就存在、从未在任何 Windows 机器上成功执行过（E0 的深层前提）。
    let sc = r#"$sc = "$env:SystemRoot\System32\sc.exe""#;
    let icacls = r#"$icacls = "$env:SystemRoot\System32\icacls.exe""#;
    format!(
        "$ErrorActionPreference = 'Stop'\n\
{sc}\n\
{icacls}\n\
$support = '{support_q}'\n\
$tokenFile = '{token_file_q}'\n\
$helperSrc = '{exe_q}'\n\
$helperDst = '{helper_dst_q}'\n\
$helperBackup = '{helper_backup_q}'\n\
$tokenBackup = '{token_backup_q}'\n\
$bp = '{bin_path_q}'\n\
New-Item -ItemType Directory -Force -Path $support | Out-Null\n\
# 锁目录 ACL：去继承、仅 SYSTEM/Administrators 完全控制并 (OI)(CI) 下传 → token/exe 出生即 SYSTEM/Admin 私有。\n\
& $icacls $support /inheritance:r | Out-Null\n\
& $icacls $support /grant:r \"SYSTEM:(OI)(CI)(F)\" \"Administrators:(OI)(CI)(F)\" | Out-Null\n\
# 升级事务快照：属性来自 Win32_Service，不解析受系统显示语言影响的 sc qc 文本。\n\
$oldService = Get-CimInstance -ClassName Win32_Service -Filter \"Name='{service}'\" -ErrorAction SilentlyContinue\n\
$serviceExisted = $null -ne $oldService\n\
$oldBinPath = if ($serviceExisted) {{ $oldService.PathName }} else {{ $null }}\n\
$oldStartMode = if ($serviceExisted) {{ $oldService.StartMode }} else {{ $null }}\n\
$oldWasRunning = $serviceExisted -and $oldService.State -eq 'Running'\n\
$hadHelper = Test-Path -LiteralPath $helperDst\n\
$hadToken = Test-Path -LiteralPath $tokenFile\n\
Remove-Item -Force -Path $helperBackup,$tokenBackup -ErrorAction SilentlyContinue\n\
if ($hadHelper) {{ Copy-Item -LiteralPath $helperDst -Destination $helperBackup -Force }}\n\
if ($hadToken) {{ Copy-Item -LiteralPath $tokenFile -Destination $tokenBackup -Force }}\n\
try {{\n\
# 先删残留旧 token 再写（旧 Admin 只读会拒 Set-Content 覆盖；经目录 FILE_DELETE_CHILD 删旧不受其自身 DACL 阻挡）。\n\
Remove-Item -Force -Path $tokenFile -ErrorAction SilentlyContinue\n\
Set-Content -Path $tokenFile -Value '{token_q}' -NoNewline -Encoding ascii\n\
& $sc stop {service} 2>$null | Out-Null\n\
& $sc delete {service} 2>$null | Out-Null\n\
# sc delete 异步标记删除 → 轮询等服务真消失，否则 New-Service 撞 1072。\n\
$deadline = (Get-Date).AddSeconds(15)\n\
while ((Get-Service -Name {service} -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {{ Start-Sleep -Milliseconds 300 }}\n\
# 外置复制（退避重试兜解锁窗口竞态）。\n\
$copied = $false\n\
for ($i = 0; $i -lt 10 -and -not $copied; $i++) {{\n\
  try {{ Copy-Item -LiteralPath $helperSrc -Destination $helperDst -Force; $copied = $true }}\n\
  catch {{ Start-Sleep -Milliseconds 300 }}\n\
}}\n\
if (-not $copied) {{ throw \"复制 helper.exe 到 ProgramData 失败（旧服务二进制可能仍被占用，请稍后重试或重启后再装）\" }}\n\
& $icacls $helperDst /inheritance:r | Out-Null\n\
& $icacls $helperDst /grant:r \"SYSTEM:(F)\" \"Administrators:(F)\" | Out-Null\n\
# New-Service 退避重试（1072 窗口）：BinaryPathName 单一字符串直达 CreateService；默认 LocalSystem；Automatic 开机自启。\n\
$created = $false\n\
$lastErr = $null\n\
for ($i = 0; $i -lt 10 -and -not $created; $i++) {{\n\
  try {{ New-Service -Name {service} -BinaryPathName $bp -StartupType Automatic | Out-Null; $created = $true }}\n\
  catch {{ $lastErr = $_; Start-Sleep -Milliseconds 500 }}\n\
}}\n\
if (-not $created) {{ throw \"New-Service 失败（重试 10 次；多为 sc delete 标记删除态 1072 竞态，若持续请重启）：$($lastErr.Exception.Message)\" }}\n\
# W20 自愈（对齐 mac plist KeepAlive=true / linux unit Restart=on-failure——Windows 此前是唯一没配自愈的平台）：\n\
# ① IU 启动权：默认服务 DACL 只给交互用户查询权（.207 实测 CCLCSWLOCRRC，无 RP=SERVICE_START），\n\
#    未提权 app 拉不起停着的服务（app 侧恢复腿的硬前提）→ 显式补授 RP（仅 start，不授 stop/改配置；\n\
#    其余 ACE 与默认逐字一致，SACL 保留）。与管道 SDDL 授 IU 读写同一威胁模型：多给的只是「让默认\n\
#    本就开机自启的服务提前起来」，无任何新能力。\n\
& $sc sdset {service} \"D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)(A;;RP;;;IU)S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)\" | Out-Null\n\
# 🔴 EAP=Stop 拦不住外部程序非零退出（PS 5.1 无 PSNativeCommandUseErrorActionPreference），\n\
# 这两步是 W20 双层自愈的硬前提，静默失败 = 装完看着成功、自愈却全没配上 → 必须显式查退出码。\n\
if ($LASTEXITCODE -ne 0) {{ throw \"sc sdset 授 IU 启动权失败（退出码 $LASTEXITCODE）\" }}\n\
# ② SCM 失败恢复：进程被任务管理器结束/异常退出 → SCM 5s 后自动重启；reset= 86400=失败计数 24h\n\
#    归零，三段 restart 覆盖连续误杀。sc 语法注意：等号后必须带一个空格。\n\
& $sc failure {service} reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null\n\
if ($LASTEXITCODE -ne 0) {{ throw \"sc failure 自愈配置失败（退出码 $LASTEXITCODE）\" }}\n\
& $sc start {service} | Out-Null\n\
if ($LASTEXITCODE -ne 0) {{ throw \"sc start helper service failed\" }}\n\
# commit：新服务已成功首启，旧文件才失去回滚价值。\n\
Remove-Item -Force -Path $helperBackup,$tokenBackup -ErrorAction SilentlyContinue\n\
}} catch {{\n\
  $installError = $_\n\
  $ErrorActionPreference = 'SilentlyContinue'\n\
  & $sc stop {service} 2>$null | Out-Null\n\
  & $sc delete {service} 2>$null | Out-Null\n\
  $rollbackDeadline = (Get-Date).AddSeconds(15)\n\
  while ((Get-Service -Name {service} -ErrorAction SilentlyContinue) -and (Get-Date) -lt $rollbackDeadline) {{ Start-Sleep -Milliseconds 300 }}\n\
  if ($hadHelper -and (Test-Path -LiteralPath $helperBackup)) {{\n\
    Copy-Item -LiteralPath $helperBackup -Destination $helperDst -Force\n\
  }} elseif (-not $hadHelper) {{\n\
    Remove-Item -Force -Path $helperDst -ErrorAction SilentlyContinue\n\
  }}\n\
  if ($hadToken -and (Test-Path -LiteralPath $tokenBackup)) {{\n\
    Copy-Item -LiteralPath $tokenBackup -Destination $tokenFile -Force\n\
  }} elseif (-not $hadToken) {{\n\
    Remove-Item -Force -Path $tokenFile -ErrorAction SilentlyContinue\n\
  }}\n\
  if ($serviceExisted) {{\n\
    $oldStartType = switch ($oldStartMode) {{ 'Disabled' {{ 'Disabled' }} 'Manual' {{ 'Manual' }} default {{ 'Automatic' }} }}\n\
    New-Service -Name {service} -BinaryPathName $oldBinPath -StartupType $oldStartType | Out-Null\n\
    & $sc sdset {service} \"D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)(A;;RP;;;IU)S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)\" | Out-Null\n\
    & $sc failure {service} reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null\n\
    if ($oldWasRunning) {{ & $sc start {service} | Out-Null }}\n\
  }}\n\
  # 失败时保留 .rollback：自动恢复若被外部文件锁阻断，管理员仍有可恢复副本。\n\
  $ErrorActionPreference = 'Stop'\n\
  throw $installError\n\
}}\n",
        support_q = ps_quote(support),
        token_file_q = ps_quote(&token_file),
        exe_q = ps_quote(&exe),
        helper_dst_q = ps_quote(&helper_dst),
        helper_backup_q = ps_quote(&helper_backup),
        token_backup_q = ps_quote(&token_backup),
        bin_path_q = ps_quote(&bin_path),
        token_q = ps_quote(token),
        icacls = icacls,
        sc = sc,
        service = WIN_SERVICE_NAME,
    )
}

/// win 卸载脚本（PowerShell）。忠实迁自 WindowsServiceHelper.ts:510-517。
/// $env: 同 install 走双引号变量 + 裸调用（单引号字面量病 2026-08-19 一并修；本脚本
/// EAP=SilentlyContinue，病发时静默什么都不卸——「卸载点了没反应」的隐性形态）。
fn build_win_uninstall_script() -> String {
    let sc = r#"$sc = "$env:SystemRoot\System32\sc.exe""#;
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
{sc}\n\
& $sc stop {service} 2>$null | Out-Null\n\
Start-Sleep -Milliseconds 300\n\
& $sc delete {service} 2>$null | Out-Null\n\
Remove-Item -Recurse -Force -Path '{support_q}' -ErrorAction SilentlyContinue\n",
        sc = sc,
        service = WIN_SERVICE_NAME,
        support_q = ps_quote(WIN_SUPPORT_DIR),
    )
}

// ===== 默认 SysOps（生产实现，跨平台可编译）=====

/// `sc.exe <args>`，**带 `CREATE_NO_WINDOW`**（winbase.h `0x0800_0000`）。
///
/// 本 crate 是库，跑在宿主 GUI 进程里（`src-tauri/src/main.rs` 的 `windows_subsystem = "windows"`
/// ⇒ 进程自身无控制台）。无控制台的父进程起 console 子系统程序时，`CreateProcess` 会**新分配一个
/// 控制台窗口** —— 装/卸/启停/探活 helper 服务每次都在用户桌面上闪一个黑框。
/// std **没有**任何隐含抑制，必须显式给这个标志（同款先例：`helper/src/platform/windows/winproc/win.rs`）。
///
/// 四个 `sc` 调用点全部经此构造：新增调用点若绕开它，`sc_calls_never_pop_a_console_window` 会红。
#[cfg(target_os = "windows")]
fn sc_command(args: [&str; 2]) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("sc");
    cmd.args(args).creation_flags(0x0800_0000);
    cmd
}

/// 生产 SysOps：用 std::fs + Command 做真系统操作。
///
/// mac/linux/win 的服务启停命令各异，这里按 target 分支。测试用 MockSysOps。
pub struct StdSysOps;

impl SysOps for StdSysOps {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn start_service(&self, label: &str) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let plist = format!("/Library/LaunchDaemons/{label}.plist");
            std::process::Command::new("launchctl")
                .args(["bootstrap", "system", &plist])
                .status()
                .map_err(|e| format!("launchctl bootstrap 失败: {e}"))?
                .success()
                .then_some(())
                .ok_or_else(|| "launchctl bootstrap 非零退出".to_owned())
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("systemctl")
                .args(["start", label])
                .status()
                .map_err(|e| format!("systemctl start 失败: {e}"))?
                .success()
                .then_some(())
                .ok_or_else(|| "systemctl start 非零退出".to_owned())
        }
        #[cfg(target_os = "windows")]
        {
            sc_command(["start", label])
                .status()
                .map_err(|e| format!("sc start 失败: {e}"))?
                .success()
                .then_some(())
                .ok_or_else(|| "sc start 非零退出".to_owned())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = label;
            Err("不支持的平台".to_owned())
        }
    }

    fn stop_service(&self, label: &str) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let plist = format!("/Library/LaunchDaemons/{label}.plist");
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", "system", &plist])
                .status();
            Ok(()) // bootout 即便未加载也忽略（上游 `2>/dev/null || true`）
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("systemctl")
                .args(["stop", label])
                .status();
            Ok(())
        }
        #[cfg(target_os = "windows")]
        {
            let _ = sc_command(["stop", label]).status();
            Ok(())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = label;
            Err("不支持的平台".to_owned())
        }
    }

    fn is_loaded(&self, label: &str) -> bool {
        #[cfg(target_os = "macos")]
        {
            // Polaris: launchctl print system/<LABEL> 退出码 0=已加载（HelperManager.ts:142-148）
            std::process::Command::new("launchctl")
                .args(["print", &format!("system/{label}")])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("systemctl")
                .args(["is-active", "--quiet", label])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(target_os = "windows")]
        {
            sc_command(["query", label])
                .output()
                .map(|o| {
                    let out = String::from_utf8_lossy(&o.stdout);
                    out.contains("RUNNING")
                })
                .unwrap_or(false)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = label;
            false
        }
    }

    fn service_exists(&self, label: &str) -> bool {
        #[cfg(target_os = "macos")]
        {
            // launchd：服务定义即 plist 落盘；已 bootout 但 plist 还在也算装过。
            std::path::Path::new(&format!("/Library/LaunchDaemons/{label}.plist")).exists()
        }
        #[cfg(target_os = "linux")]
        {
            // `systemctl cat` 只要 unit 存在就 0（停着也 0），is-active 会漏掉停着的。
            std::process::Command::new("systemctl")
                .args(["cat", label])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        #[cfg(target_os = "windows")]
        {
            // `sc query` 服务不存在时退出码 1060；存在则 0（无论 RUNNING/STOPPED）。
            // 故判退出码而非像 is_loaded 那样扫 stdout 里的 "RUNNING"。
            sc_command(["query", label])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = label;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockStream;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    /// mock SysOps：可预设路径存在性 + 服务状态。
    #[derive(Default)]
    struct MockSysOps {
        exists_paths: HashSet<PathBuf>,
        loaded: bool,
        /// SCM/launchd/systemd 里**已注册**的服务标识（可停着）。与 `loaded`（正在运行）正交：
        /// Windows 的 `is_installed` 判的是注册而非运行。
        registered_services: HashSet<String>,
        /// W20：拉起是否失败（模拟二进制被删 / DACL 无 SERVICE_START）。
        start_fails: bool,
        start_calls: Arc<Mutex<Vec<String>>>,
        stop_calls: Arc<Mutex<Vec<String>>>,
        /// `is_loaded` 收到的 label —— **必须记**：早先它只返 `self.loaded`、丢掉 label，
        /// 于是「`is_loaded` 硬传 SERVICE_LABEL」这个变异在 Windows 上无人能杀
        /// （`start`/`stop` 有门、`is_loaded` 没有 = 三缺一）。
        loaded_calls: Arc<Mutex<Vec<String>>>,
    }

    impl SysOps for MockSysOps {
        fn exists(&self, path: &Path) -> bool {
            self.exists_paths.contains(path)
        }
        fn start_service(&self, label: &str) -> Result<(), String> {
            self.start_calls.lock().unwrap().push(label.to_owned());
            if self.start_fails {
                return Err("mock: 拉起失败（模拟二进制被删 / 无 SERVICE_START）".to_owned());
            }
            Ok(())
        }
        fn stop_service(&self, label: &str) -> Result<(), String> {
            self.stop_calls.lock().unwrap().push(label.to_owned());
            Ok(())
        }
        fn is_loaded(&self, label: &str) -> bool {
            self.loaded_calls.lock().unwrap().push(label.to_owned());
            self.loaded
        }
        fn service_exists(&self, label: &str) -> bool {
            self.registered_services.contains(label)
        }
    }

    /// mock connector：返回预置 MockStream。
    #[derive(Clone)]
    struct MockConnector {
        streams: Arc<Mutex<Vec<MockStream>>>,
    }

    impl Connector for MockConnector {
        fn connect(&self) -> Result<Box<dyn crate::transport::ConnectionStream>, ClientError> {
            let mut g = self.streams.lock().unwrap();
            if g.is_empty() {
                return Err(ClientError::Connect("no mock".into()));
            }
            Ok(Box::new(g.remove(0)))
        }
    }

    fn manager(platform: Platform, sysops: MockSysOps) -> HelperManager {
        HelperManager::new(
            platform,
            PathBuf::from("/tmp/helper-client.token"),
            Box::new(sysops),
        )
    }

    /// 构造 manager + 写好 token 文件（让 compute_status 跑到 ping 探测阶段）。
    fn manager_with_token(
        platform: Platform,
        sysops: MockSysOps,
    ) -> (HelperManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        crate::token::write_token_content(&token_path, "TOK").unwrap();
        let m = HelperManager::new(platform, token_path, Box::new(sysops));
        (m, dir)
    }

    /// 当前源码 helper 的真实 pong wire；测试不要再手拼一份漏掉 build identity 的“半握手”。
    fn current_pong_wire() -> Vec<u8> {
        format!(
            "{}\n",
            Response::Ok(ResponseKind::Pong(Pong::current(0))).to_wire_line()
        )
        .into_bytes()
    }

    #[test]
    fn install_paths_mac_match_polaris() {
        // Polaris HelperManager.ts:30,33-35
        let p = InstallPaths::mac();
        assert_eq!(
            p.binary,
            PathBuf::from("/Library/PrivilegedHelperTools/com.polaris.helper")
        );
        assert_eq!(
            p.descriptor,
            Some(PathBuf::from(
                "/Library/LaunchDaemons/com.polaris.helper.plist"
            ))
        );
        assert!(p.socket.to_string_lossy().contains("helper.sock"));
    }

    #[test]
    fn install_paths_for_platform() {
        assert_eq!(
            InstallPaths::for_platform(Platform::Mac),
            InstallPaths::mac()
        );
        assert_eq!(
            InstallPaths::for_platform(Platform::Linux),
            InstallPaths::linux()
        );
        assert_eq!(
            InstallPaths::for_platform(Platform::Win),
            InstallPaths::win()
        );
    }

    /// W17 防回潮（2026-08-20 订正为 Platform 键控形态）：is_installed 的 **Win 平台**必须
    /// 「SCM 单证据」，不得回到文件 stat——安装脚本的 ACL 锁下未提权 app 恒 false
    /// （2026-08-19 .207 实测 Test-Path=False 而 sc query=0）。证据集按 Platform 枚举分派
    /// 而非编译目标 cfg（cfg 形态曾让 CI win 腿六测全红：win 目标上 Mac 平台 mock 也被拽进
    /// SCM 分支；push 只跑 ubuntu 腿不可见，全矩阵 dispatch 才暴露）。
    #[test]
    fn win_is_installed_uses_scm_evidence_not_the_acl_locked_file() {
        let src = include_str!("manager.rs");
        let at = src.find("pub fn is_installed(").expect("is_installed 消失");
        // 从函数起点向后找**下一个兄弟文档注释**作切片终点（文件前部有同名文案，全局 find 会倒挂）
        let end = src[at..].find("\n    /// ").map_or(src.len(), |i| at + i);
        let body = &src[at..end];
        let win_at = body
            .find("Platform::Win => self.sysops.service_exists")
            .expect("is_installed 缺 Win 平台 SCM 单证据臂");
        assert!(
            !body[..win_at].contains("sysops.exists"),
            "Win 臂早退之前不得有文件 stat（W17 复发：ACL 锁下未提权恒 false）"
        );
        assert!(
            !body.contains("#[cfg("),
            "证据分派不得再按编译目标 cfg（Platform 键控，测试须宿主无关）"
        );
    }

    /// W10 跟进项钉扎：装后就绪轮询窗 ≥ 10s（20×500ms）——.207 首装实测 3s 窗口
    /// 把快照定格在未就绪、卡片停在安装前旧态。收紧需带新的真机计时依据。
    #[test]
    fn ready_poll_window_covers_scm_cold_start() {
        assert_eq!(READY_POLL_ATTEMPTS, 20);
        assert_eq!(READY_POLL_DELAY, Duration::from_millis(500));
    }

    #[test]
    fn is_installed_requires_both_binary_and_descriptor() {
        // Polaris filesPresent: HELPER_DEST && PLIST_PATH（HelperManager.ts:202）
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        // 只放 binary，不放 descriptor → 未装
        sysops.exists_paths.insert(paths.binary.clone());
        let m = manager(Platform::Mac, sysops);
        assert!(!m.is_installed());
    }

    #[test]
    fn is_installed_true_when_both_present() {
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.clone().expect("mac/linux 有描述符文件"));
        let m = manager(Platform::Mac, sysops);
        assert!(m.is_installed());
    }

    /// Windows 的「第二条证据」是 SCM 服务已注册，不是描述符文件。
    ///
    /// 早先 `InstallPaths::win()` 塞了个从不创建的 `helper-service.yml` 且被 stat，
    /// 导致 Windows 上 `is_installed` 恒 false → `compute_status_with_client` 短路成全 false、
    /// 连管道都不 ping → helper 卡片恒显示未安装、TUN 起核门每次弹提权引导且装完复检仍判未装。
    ///
    /// **变异**：把 `is_installed` 的 `None` 腿改回 stat `descriptor` → 首条断言转红。
    #[test]
    fn is_installed_on_windows_reads_scm_service_not_a_descriptor_file() {
        let paths = InstallPaths::win();
        assert!(
            paths.descriptor.is_none(),
            "Windows 服务定义在 SCM，磁盘无描述符文件"
        );

        // exe 在位 + 服务已注册 → 已安装
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        assert!(manager(Platform::Win, sysops).is_installed());

        // exe 在位但服务没注册（装了一半 / 服务被删）→ 未安装
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        assert!(!manager(Platform::Win, sysops).is_installed());

        // 服务在但 exe 没了 → Win 平台 SCM 单证据 ⇒ 判已装（宿主无关，2026-08-20 订正为
        // Platform 键控后不再 cfg 拆分）。ping 随后挂 → needs_repair →「点一下修复」而非
        // 「从未装过」——与 is_installed 头注「不得把可修复态误报成未安装态」同一条设计
        // 原则；重装脚本 Copy-Item -Force 覆盖缺失文件。
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        assert!(manager(Platform::Win, sysops).is_installed());
    }

    /// 「已注册」与「正在运行」正交：服务装了但停着，仍须判已安装。
    ///
    /// 若这里图省事复用 `is_loaded`（要求 RUNNING），一台 helper 停着的机器会被判成从没装过，
    /// 直接丢掉 `needs_repair` 可修复态 —— 用户看到的是「未安装」而非「点一下修复」。
    #[test]
    fn is_installed_on_windows_true_even_when_service_stopped() {
        let paths = InstallPaths::win();
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        sysops.loaded = false; // 停着
        let m = manager(Platform::Win, sysops);
        assert!(m.is_installed(), "装了但停着仍算已安装");
        assert!(!m.is_loaded(), "但不算正在运行");
    }

    /// 生命周期操作必须用**本平台**的服务标识。
    ///
    /// 早先 `is_loaded`/`start`/`stop` 一律硬传 `SERVICE_LABEL`（`com.polaris.helper`），
    /// 而 Windows 装出来的服务叫 `PolarisHelper` ⇒ `sc query/start/stop` 全部打在不存在的服务上。
    ///
    /// **变异**：把这三个方法改回硬传 `SERVICE_LABEL` → 本条转红。
    #[test]
    fn lifecycle_uses_platform_service_label() {
        for (platform, want) in [
            (Platform::Win, WIN_SERVICE_NAME),
            (Platform::Mac, SERVICE_LABEL),
            (Platform::Linux, SERVICE_LABEL),
        ] {
            let start_calls = Arc::new(Mutex::new(vec![]));
            let stop_calls = Arc::new(Mutex::new(vec![]));
            let loaded_calls = Arc::new(Mutex::new(vec![]));
            let sysops = MockSysOps {
                exists_paths: HashSet::new(),
                loaded: true,
                registered_services: HashSet::new(),
                start_fails: false,
                start_calls: start_calls.clone(),
                stop_calls: stop_calls.clone(),
                loaded_calls: loaded_calls.clone(),
            };
            let m = manager(platform, sysops);
            m.start().unwrap();
            m.stop().unwrap();
            let _ = m.is_loaded();
            // 三个方法都要断言，缺一即留逃逸面（实测：只断 start/stop 时，
            // 「is_loaded 硬传 SERVICE_LABEL」这个变异存活）。
            assert_eq!(
                (*start_calls.lock().unwrap()).clone(),
                vec![want.to_owned()],
                "{platform:?} 的 start 应作用于 {want}"
            );
            assert_eq!(
                (*stop_calls.lock().unwrap()).clone(),
                vec![want.to_owned()],
                "{platform:?} 的 stop 应作用于 {want}"
            );
            assert_eq!(
                (*loaded_calls.lock().unwrap()).clone(),
                vec![want.to_owned()],
                "{platform:?} 的 is_loaded 应作用于 {want}"
            );
        }
    }

    #[test]
    fn status_not_installed_returns_empty() {
        // Polaris computeStatus: !filesPresent → 全 false（HelperManager.ts:174-184）
        let m = manager(Platform::Mac, MockSysOps::default());
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![])),
        };
        let status = m.compute_status_with_client(&HelperClient::new(
            Box::new(connector),
            Platform::Mac,
            "",
        ));
        assert!(!status.installed);
        assert!(!status.ready);
    }

    #[test]
    fn status_installed_but_no_token_needs_repair() {
        // installed 但 token 缺失 → needsRepair（token 文件读不到）
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.clone().expect("mac/linux 有描述符文件"));
        let m = HelperManager::new(
            Platform::Mac,
            PathBuf::from("/nonexistent/path/token"), // 不存在
            Box::new(sysops),
        );
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![])),
        };
        let status = m.compute_status_with_client(&HelperClient::new(
            Box::new(connector),
            Platform::Mac,
            "",
        ));
        assert!(status.installed);
        assert!(!status.ready);
        assert!(status.needs_repair);
    }

    #[test]
    fn status_ready_when_proto_above_min_usable() {
        // helper 广告统一的 CURRENT → ready（不再是 上游的 9 ≥ 4）。
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        crate::token::write_token_content(&token_path, "TOK").unwrap();
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.clone().expect("mac/linux 有描述符文件"));
        let m = HelperManager::new(Platform::Mac, token_path, Box::new(sysops));
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::with_response(
                current_pong_wire(),
            )])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Mac, "TOK");
        let status = m.compute_status_with_client(&client);
        assert!(status.installed);
        assert!(status.ready);
        assert_eq!(
            status.version,
            Some(polaris_helper_proto::proto_version::CURRENT)
        );
        // 自家 helper == 本 build 期望版本 → 不 upgradeable、不 needs_repair。
        assert!(!status.upgradeable);
        assert!(!status.needs_repair);
    }

    #[test]
    fn status_same_proto_without_build_identity_is_ready_but_upgradeable() {
        // .207 现场旧 helper：protocol v1 与随包 helper 相同，但 pong 没 build 字段。它仍可用，
        // 不能误报 needsRepair；同时必须进入既有五语种升级流，否则部署漂移会永久滞留。
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.expect("mac 有描述符文件"));
        let (m, _dir) = manager_with_token(Platform::Mac, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::with_response(
                format!(
                    "OK pong uid=0 v{}\n",
                    polaris_helper_proto::proto_version::CURRENT
                )
                .into_bytes(),
            )])),
        };
        let status = m.compute_status_with_client(&HelperClient::new(
            Box::new(connector),
            Platform::Mac,
            "TOK",
        ));
        assert!(status.ready, "同 proto 旧 helper 仍可用");
        assert!(status.upgradeable, "缺 build identity 必须识别为旧 helper");
        assert!(!status.needs_repair, "升级态不是损坏态");
        assert_eq!(status.build_id, None);
    }

    #[test]
    fn status_same_proto_with_different_build_identity_is_upgradeable() {
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.expect("mac 有描述符文件"));
        let (m, _dir) = manager_with_token(Platform::Mac, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::with_response(
                format!(
                    "OK pong uid=0 v{} build=older-package\n",
                    polaris_helper_proto::proto_version::CURRENT
                )
                .into_bytes(),
            )])),
        };
        let status = m.compute_status_with_client(&HelperClient::new(
            Box::new(connector),
            Platform::Mac,
            "TOK",
        ));
        assert!(status.ready);
        assert!(status.upgradeable);
        assert_eq!(status.build_id.as_deref(), Some("older-package"));
        assert!(!status.needs_repair);
    }

    #[test]
    fn upgradeable_window_is_empty_while_min_usable_equals_current() {
        // 旧测 `status_upgradeable_when_proto_between_min_and_expected` 靠 上游的 MIN_USABLE(4)
        // < EXPECTED(9) 撑出一个「够用但偏旧」的窗口（v5 落在里面）。统一 v1 后 MIN_USABLE ==
        // CURRENT ⇒ 窗口为空——这不是缺陷，是「Polaris 尚无第二代 helper」的事实。
        // `CURRENT` 一旦 +1，窗口自动打开，届时本测该改回「窗口内的版本判 upgradeable」。
        for v in 0..=(polaris_helper_proto::proto_version::CURRENT + 2) {
            let ready = v >= MIN_USABLE_PROTO;
            let upgradeable = ready && v < expected_proto();
            assert!(!upgradeable, "v{v}：当前无更新代次，不该判可升级");
        }
    }

    #[test]
    fn status_needs_repair_when_proto_below_min_usable() {
        // proto=0 < MIN_USABLE(1) → !ready → needsRepair（唯一低于门槛的取值：解析失败也落 0）
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.clone().expect("mac/linux 有描述符文件"));
        let (m, _dir) = manager_with_token(Platform::Mac, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::with_response(
                b"OK pong uid=0 v0\n".to_vec(),
            )])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Mac, "TOK");
        let status = m.compute_status_with_client(&client);
        assert!(status.installed);
        assert!(!status.ready);
        assert!(status.needs_repair);
    }

    // ── W20：status_with_recovery（「装了但停着」分型拉起，Windows 手杀 helper 的自愈腿）──

    /// Win 分身：注册 + 二进制在 + token 在 + 停着 + ping 挂 → 拉起一次 + 复核 ready。
    /// 变异锁：删恢复腿 / 删 is_loaded 分型 / 拉起后不复核，本条或下两条之一必转红。
    /// （is_installed 2026-08-20 起按 Platform 键控证据集：Win 平台在任何宿主都走 SCM 单证据，
    /// 故本组测试宿主无关地驱动完整恢复逻辑。）
    #[test]
    fn recovery_pulls_up_stopped_service_and_becomes_ready() {
        let paths = InstallPaths::win();
        let start_calls = Arc::new(Mutex::new(Vec::new()));
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        sysops.start_calls = start_calls.clone();
        let (m, _dir) = manager_with_token(Platform::Win, sysops);
        // 第一次 ping 挂（broken 流）→ 拉起后复核 ping 通（pong 流）。
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![
                MockStream::broken(std::io::ErrorKind::ConnectionAborted),
                MockStream::with_response(current_pong_wire()),
            ])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Win, "TOK");
        let status = m.status_with_recovery_poll(&client, 3, Duration::from_millis(1));
        assert_eq!(
            *start_calls.lock().unwrap(),
            vec![paths.service_label.to_owned()]
        );
        assert!(status.installed);
        assert!(status.ready, "拉起后复核应就绪");
        assert!(!status.needs_repair);
        assert_eq!(
            status.version,
            Some(polaris_helper_proto::proto_version::CURRENT)
        );
    }

    /// 分型：跑着（is_loaded）仍 ping 不通 = 结构性问题 → 不拉服务，交回修复流。
    #[test]
    fn recovery_skips_start_when_running_but_unreachable() {
        let paths = InstallPaths::win();
        let start_calls = Arc::new(Mutex::new(Vec::new()));
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        sysops.loaded = true;
        sysops.start_calls = start_calls.clone();
        let (m, _dir) = manager_with_token(Platform::Win, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::broken(
                std::io::ErrorKind::ConnectionAborted,
            )])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Win, "TOK");
        let status = m.status_with_recovery_poll(&client, 2, Duration::from_millis(1));
        assert!(
            start_calls.lock().unwrap().is_empty(),
            "跑着仍不通是结构问题，拉服务是误动作"
        );
        assert!(status.needs_repair);
    }

    /// 分型：token 缺失 → 拉起也过不了鉴权，不白拉。
    #[test]
    fn recovery_skips_start_when_token_missing() {
        let paths = InstallPaths::win();
        let start_calls = Arc::new(Mutex::new(Vec::new()));
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        sysops.start_calls = start_calls.clone();
        let m = manager(Platform::Win, sysops); // 不写 token
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::broken(
                std::io::ErrorKind::ConnectionAborted,
            )])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Win, "");
        let status = m.status_with_recovery_poll(&client, 2, Duration::from_millis(1));
        assert!(start_calls.lock().unwrap().is_empty(), "无 token 不白拉");
        assert!(status.needs_repair);
    }

    /// 拉起失败（二进制被删 / DACL 无 SERVICE_START）→ 如实维持 needs_repair，不粉饰。
    ///
    /// 预置一条 pong 流作「诱饵」：正确的失败路径**不该消费它**（start Err 即返，不轮询）；
    /// 若有人把 start 失败吞成 Ok（或删掉 MockSysOps 的失败旋钮），轮询会吃到 pong 变 ready，
    /// 本条转红——否则它会退化成与 never_binds 用例不可区分的弱断言（变异电池 M9 实证）。
    #[test]
    fn recovery_maintains_repair_when_start_fails() {
        let paths = InstallPaths::win();
        let start_calls = Arc::new(Mutex::new(Vec::new()));
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        sysops.start_fails = true;
        sysops.start_calls = start_calls.clone();
        let (m, _dir) = manager_with_token(Platform::Win, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![
                MockStream::broken(std::io::ErrorKind::ConnectionAborted),
                MockStream::with_response(current_pong_wire()),
            ])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Win, "TOK");
        let status = m.status_with_recovery_poll(&client, 2, Duration::from_millis(1));
        assert_eq!(start_calls.lock().unwrap().len(), 1, "确实试拉了一次");
        assert!(status.needs_repair, "拉不起 = 真坏，该修不该粉饰");
    }

    /// 拉起成功但管道始终不绑（如起即崩）→ 轮询耗尽后维持 needs_repair。
    #[test]
    fn recovery_maintains_repair_when_service_never_binds() {
        let paths = InstallPaths::win();
        let mut sysops = MockSysOps::default();
        sysops
            .registered_services
            .insert(paths.service_label.to_owned());
        let (m, _dir) = manager_with_token(Platform::Win, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![
                MockStream::broken(std::io::ErrorKind::ConnectionAborted),
                MockStream::broken(std::io::ErrorKind::ConnectionAborted),
                MockStream::broken(std::io::ErrorKind::ConnectionAborted),
            ])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Win, "TOK");
        let status = m.status_with_recovery_poll(&client, 2, Duration::from_millis(1));
        assert!(status.needs_repair, "复核仍不通 → 维持修复态");
    }

    #[test]
    fn status_needs_repair_when_ping_fails() {
        // ping 连接失败（helper 未跑）→ version None → needsRepair
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.clone().expect("mac/linux 有描述符文件"));
        let (m, _dir) = manager_with_token(Platform::Mac, sysops);
        // connector 返回空（连接失败 → send 报错 → version None）
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Mac, "TOK");
        let status = m.compute_status_with_client(&client);
        assert!(status.installed);
        assert!(!status.ready);
        assert!(status.needs_repair);
    }

    #[test]
    fn start_stop_delegate_to_sysops() {
        let start_calls = Arc::new(Mutex::new(vec![]));
        let stop_calls = Arc::new(Mutex::new(vec![]));
        let sysops = MockSysOps {
            exists_paths: HashSet::new(),
            loaded: true,
            registered_services: HashSet::new(),
            start_calls: start_calls.clone(),
            stop_calls: stop_calls.clone(),
            ..Default::default()
        };
        let m = manager(Platform::Mac, sysops);
        m.start().unwrap();
        m.stop().unwrap();
        assert_eq!(
            (*start_calls.lock().unwrap()).clone(),
            vec![SERVICE_LABEL.to_owned()]
        );
        assert_eq!(
            (*stop_calls.lock().unwrap()).clone(),
            vec![SERVICE_LABEL.to_owned()]
        );
    }

    #[test]
    fn is_loaded_delegates() {
        let sysops = MockSysOps {
            loaded: true,
            ..Default::default()
        };
        let m = manager(Platform::Mac, sysops);
        assert!(m.is_loaded());
    }

    #[test]
    fn prepare_token_reuses_existing() {
        // Polaris install 复用已有 token（HelperManager.ts:478-482）
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token");
        // 预置 token
        token::write_token_content(&token_path, "existing-tok").unwrap();
        let m = HelperManager::new(Platform::Mac, token_path, Box::new(MockSysOps::default()));
        let t = m.prepare_token().unwrap();
        assert_eq!(t, "existing-tok");
    }

    #[test]
    fn prepare_token_generates_when_missing() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token");
        let m = HelperManager::new(
            Platform::Mac,
            token_path.clone(),
            Box::new(MockSysOps::default()),
        );
        let t = m.prepare_token().unwrap();
        assert_eq!(t.len(), 32, "新 token 须 32 hex 字符");
        assert!(token_path.exists());
    }

    #[test]
    fn clear_token_removes_file() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token");
        token::write_token_content(&token_path, "tok").unwrap();
        assert!(token_path.exists());
        let m = HelperManager::new(
            Platform::Mac,
            token_path.clone(),
            Box::new(MockSysOps::default()),
        );
        m.clear_token();
        assert!(!token_path.exists());
    }

    // ── protoVersion 期望值（前提已换：不再是 mac=9 的三谱系）────────────────────
    // 旧测 `expected_proto_mac_is_9` / `expected_proto_matches_helper_proto_constants` 锁的是 上游
    // 的 9/5/1。那前提对 Polaris 不成立（无旧版 helper 需被认出），已随常量统一一并推翻。

    #[test]
    fn expected_proto_is_unified_current() {
        assert_eq!(
            expected_proto(),
            polaris_helper_proto::proto_version::CURRENT
        );
    }

    // `black_box` 挡住常量折叠：两个阈值都是 const，直写会被 clippy::assertions_on_constants 判为
    // 恒真断言。用 const 块断言也能通过，但那样失败形态是**编译不过**、丢掉下面这段解释性 message；
    // 这里要的正是「门红了并告诉你为什么」。
    #[test]
    fn min_usable_not_above_current() {
        // 这条守的是「统一版本号」最凶的连带：门槛留在 上游的 4、而 helper 广告 1 → 每台机器都
        // ready=false + needs_repair=true，TUN 全线不可用。门槛必须 ≤ 当前广告版本。
        let min = std::hint::black_box(MIN_USABLE_PROTO);
        let cur = std::hint::black_box(polaris_helper_proto::proto_version::CURRENT);
        assert!(
            min <= cur,
            "MIN_USABLE_PROTO({min}) > CURRENT({cur}) → 自家 helper 会被判为需修复，TUN 全线不可用"
        );
    }

    #[test]
    fn own_helper_is_ready_and_not_upgradeable() {
        // 端到端语义：本 build 的 helper 广告 CURRENT → 必须判 ready 且不提示可升级。
        // 这是 min_usable / expected_proto 两个阈值的**汇合断言**，任一改坏都会红。
        let v = std::hint::black_box(polaris_helper_proto::proto_version::CURRENT);
        assert!(v >= MIN_USABLE_PROTO, "自家 helper 必须判 ready");
        assert!(v >= expected_proto(), "自家 helper 不该被判为可升级");
    }

    // ===== 装卸流程（install/uninstall）=====
    // ClientError / EscalationOutcome / Executor 经 `use super::*` 从模块级引入。
    use crate::privilege::EscalationOutcome;

    type CapturedCalls = Arc<Mutex<Vec<Vec<String>>>>;
    type CapturedScript = Arc<Mutex<Option<String>>>;

    /// 捕获提权 argv + 读脚本文件内容（execute 时脚本尚未清理）的 mock executor。
    struct CapturingExecutor {
        calls: CapturedCalls,
        /// pkexec argv[2] 即脚本路径 → 读其内容供断言（linux 全路径可跑）。
        script_content: CapturedScript,
        result: (String, i32),
    }
    impl Executor for CapturingExecutor {
        fn execute(&self, argv: &[String]) -> Result<(String, i32), ClientError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            // pkexec: [/usr/bin/pkexec, /bin/bash, <scriptPath>]。读脚本内容（清理前）。
            if let Some(path) = argv.get(2) {
                if let Ok(c) = std::fs::read_to_string(path) {
                    *self.script_content.lock().unwrap() = Some(c);
                }
            }
            Ok(self.result.clone())
        }
    }
    fn capturing(result: (String, i32)) -> (CapturingExecutor, CapturedCalls, CapturedScript) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let content = Arc::new(Mutex::new(None));
        let e = CapturingExecutor {
            calls: calls.clone(),
            script_content: content.clone(),
            result,
        };
        (e, calls, content)
    }

    fn install_params(script_dir: PathBuf, src: PathBuf) -> InstallParams {
        InstallParams {
            src_binary: src,
            bundled_core: PathBuf::from("/app/resources/sing-box"),
            singbox_path: PathBuf::from("/app/resources/sing-box"),
            conf_dir: PathBuf::from("/home/user/.config/polaris"),
            uid: 1000,
            script_dir,
        }
    }

    /// 构造已「装好源二进制」的 manager + 可写 token_path（tempdir）。
    fn install_manager(platform: Platform, src: &Path) -> (HelperManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("helper-client.token");
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(src.to_path_buf());
        let m = HelperManager::new(platform, token_path, Box::new(sysops));
        (m, dir)
    }

    // ── 脚本内容（移植保真度 —— mutation 相关：删任一关键步骤即挂）──
    #[test]
    fn mac_install_script_has_all_steps() {
        let paths = InstallPaths::mac();
        let p = install_params(PathBuf::from("/x"), PathBuf::from("/app/helper"));
        let s = build_mac_install_script(&paths, &p, "TOKEN123");
        // 拷二进制到特权路径
        assert!(s.contains("cp \"$SRC\" \"$DEST\""), "缺 cp 二进制");
        assert!(
            s.contains("DEST='/Library/PrivilegedHelperTools/com.polaris.helper'"),
            "DEST 路径错"
        );
        // 写 root 侧 token（600）
        assert!(
            s.contains("printf '%s' 'TOKEN123' > \"$SUPPORT/helper.token\""),
            "缺写 token"
        );
        assert!(
            s.contains("chmod 600 \"$SUPPORT/helper.token\""),
            "token 权限须 600"
        );
        // 播种核（守卫 + codesign）
        assert!(
            s.contains("if [ ! -x \"$COREDIR/sing-box\" ]; then"),
            "缺核播种守卫"
        );
        assert!(s.contains("codesign --force --sign -"), "缺 codesign");
        // 写 plist（含 daemon flag）
        assert!(
            s.contains("<key>Label</key><string>com.polaris.helper</string>"),
            "plist Label 错"
        );
        assert!(
            s.contains("<string>--singbox</string>"),
            "plist 缺 --singbox flag"
        );
        assert!(
            s.contains("<string>--coredir</string>"),
            "plist 缺 --coredir flag"
        );
        // bootstrap
        assert!(
            s.contains("launchctl bootstrap system \"$PLIST\""),
            "缺 launchctl bootstrap"
        );
        assert!(s.contains("echo installed-ok"));
    }

    #[test]
    fn mac_plist_flags_match_daemon_parse_args() {
        // daemon macos/daemon.rs:parse_args 认 --singbox/--confdir/--support/--coredir；plist argv 须逐一对上。
        let plist = render_mac_plist(
            "/Library/PrivilegedHelperTools/com.polaris.helper",
            "/Library/Application Support/Polaris/core",
            "/home/u/conf",
            "/Library/Application Support/Polaris",
        );
        assert!(plist.contains("<string>--singbox</string><string>/Library/Application Support/Polaris/core/sing-box</string>"));
        assert!(plist.contains("<string>--confdir</string><string>/home/u/conf</string>"));
        assert!(plist.contains(
            "<string>--support</string><string>/Library/Application Support/Polaris</string>"
        ));
        assert!(plist.contains(
            "<string>--coredir</string><string>/Library/Application Support/Polaris/core</string>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
    }

    #[test]
    fn linux_install_script_has_all_steps() {
        let paths = InstallPaths::linux();
        let p = install_params(PathBuf::from("/x"), PathBuf::from("/app/helper"));
        let s = build_linux_install_script(&paths, &p);
        // 拷二进制
        assert!(
            s.contains(
                "install -D -o root -g root -m 0755 '/app/helper' '/usr/local/lib/polaris/helper'"
            ),
            "缺装二进制"
        );
        // 播种核守卫
        assert!(
            s.contains("if [ ! -x '/usr/local/lib/polaris/core/sing-box' ]; then"),
            "缺核播种守卫"
        );
        // 授权 uid 合并追加（不覆写）
        assert!(
            s.contains("grep -qxF '1000' '/var/lib/polaris/authorized-uids'"),
            "缺 uid 授权"
        );
        // 装 unit（含 daemon flag）
        assert!(s.contains("ExecStart=/usr/local/lib/polaris/helper --socket=/run/polaris/helper.sock --authfile=/var/lib/polaris/authorized-uids --coredir=/usr/local/lib/polaris/core"), "ExecStart flag 须对齐 daemon parse_args");
        assert!(s.contains("RuntimeDirectory=polaris"));
        // reload + enable --now
        assert!(s.contains("systemctl daemon-reload"), "缺 daemon-reload");
        assert!(
            s.contains("systemctl enable --now polaris-helper.service"),
            "缺 enable --now"
        );
        assert!(s.contains("echo polaris-helper-install-ok"));
    }

    /// **接线门**：状态探测读的路径 / 服务名，必须与安装脚本真正写的逐字一致。
    ///
    /// 这条测试的全部价值在于把两条曾经分叉的真相源钉在一起。此前两侧**各自都有测试且都绿**——
    /// `win_install_script_has_all_steps` 钉脚本落点、`is_installed_*` 用 mock 自填路径钉判定
    /// （判据与被判对象同源，是恒真的同义反复）——**唯独没人比过它们指不指同一个东西**。
    /// 于是 Windows 上 `is_installed` 恒 false。典型的「测方法体不测接线」。
    ///
    /// 顺带钉死「落点不再随源文件名漂移」：这里故意传一个**改过名**的 src_binary。
    #[test]
    fn win_install_script_targets_the_same_paths_status_probes() {
        let paths = InstallPaths::win();
        // 故意用与目标不同的源文件名：落点应恒为 WIN_HELPER_EXE，不随 src 漂移。
        let mut p = install_params(
            PathBuf::from("/x"),
            PathBuf::from(r"C:\app\renamed-helper.exe"),
        );
        p.singbox_path = PathBuf::from(r"C:\app\sing-box.exe");
        let script = build_win_install_script(&p, "WTOKEN");

        let binary = paths.binary.to_string_lossy();
        assert!(
            script.contains(&format!("$helperDst = '{binary}'")),
            "状态探测查 {binary}，安装脚本却装到别处——两条真相源又分叉了"
        );
        // 源文件名出现在 `$helperSrc` 是对的（那是拷贝来源）；要钉的是**目标**不随它漂移。
        assert!(
            !script.contains(&format!(r"{WIN_SUPPORT_DIR}\renamed-helper.exe")),
            "落点不得随源文件名漂移"
        );
        assert!(
            script.contains(&format!("New-Service -Name {}", paths.service_label)),
            "安装脚本注册的服务名与状态探测用的不一致"
        );
        // 卸载脚本也得指同一个服务，否则卸不干净、下次装撞 1072。
        assert!(
            build_win_uninstall_script().contains(&format!("delete {}", paths.service_label)),
            "卸载脚本删的服务名与状态探测不一致"
        );
    }

    #[test]
    fn win_install_script_has_all_steps() {
        let mut p = install_params(
            PathBuf::from("/x"),
            PathBuf::from(r"C:\app\polaris-helper.exe"),
        );
        p.singbox_path = PathBuf::from(r"C:\app\sing-box.exe");
        let s = build_win_install_script(&p, "WTOKEN");
        // 外置副本到 ProgramData
        assert!(
            s.contains(r"$helperDst = 'C:\ProgramData\Polaris\polaris-helper.exe'"),
            "helperDst 须外置到 ProgramData"
        );
        // 锁 ACL
        assert!(
            s.contains(r#"/grant:r "SYSTEM:(OI)(CI)(F)" "Administrators:(OI)(CI)(F)""#),
            "缺目录 ACL 锁"
        );
        // 写 token
        assert!(
            s.contains("Set-Content -Path $tokenFile -Value 'WTOKEN' -NoNewline -Encoding ascii"),
            "缺写 token"
        );
        // binPath 含 daemon flag（真双引号）
        assert!(
            s.contains(r#"--singbox "C:\app\sing-box.exe" --confdir"#),
            "binPath 缺 --singbox"
        );
        assert!(
            s.contains(r#"--support "C:\ProgramData\Polaris""#),
            "binPath 缺 --support"
        );
        // New-Service + start
        assert!(
            s.contains(
                "New-Service -Name PolarisHelper -BinaryPathName $bp -StartupType Automatic"
            ),
            "缺 New-Service"
        );
        assert!(s.contains("& $sc start PolarisHelper"), "缺 sc start");
        assert!(
            s.contains("& $icacls $support /inheritance:r"),
            "缺 icacls inheritance:r"
        );
        assert!(
            s.contains("$ErrorActionPreference = 'Stop'"),
            "缺 fail-loud"
        );
        // 🔴 病根牙（2026-08-19 提权重放首曝）：`& '$env:...'` / `& '$sc'` 这类**单引号包任何
        // $ 引用**都是字面量不展开 → CommandNotFound + EAP=Stop → 脚本必死。本脚本所有 `& `
        // 调用位都应是裸变量/裸路径，出现 `& '$` 即病（评审实证：分句带冒号的禁令是永真死针，
        // `& '$sc'` 穿透）。
        assert!(
            !s.contains("& '$"),
            "win 安装脚本出现单引号包 $ 引用的调用——CommandNotFound 必死形态"
        );
        assert!(
            s.contains(r#"$icacls = "$env:SystemRoot\System32\icacls.exe""#),
            "缺 $env: 双引号变量赋值（icacls）"
        );
        assert!(
            s.contains(r#"$sc = "$env:SystemRoot\System32\sc.exe""#),
            "缺 $env: 双引号变量赋值（sc）"
        );
    }

    /// W20：安装脚本必须配好两层自愈——① `sdset` 授 IU `SERVICE_START`（默认服务 DACL 只给交互
    /// 用户查询权，.207 实测无 RP；不补授则未提权 app 永远拉不起停着的服务）；② `sc failure`
    /// 失败恢复（任务管理器手杀/崩溃 → SCM 5s 自动重启，对齐 mac KeepAlive / linux Restart）。
    /// 两者都必须在首次 `sc start` 之前，且各带 `$LASTEXITCODE` 守卫——PS 5.1 的 EAP=Stop 拦不住
    /// 外部程序非零退出（评审 F3），静默失败会让安装「看着成功」而自愈全没配上。
    /// 变异锁：删任一行 / 删守卫 / 挪到 start 之后 → 转红。
    #[test]
    fn win_install_script_self_heals_and_grants_iu_start_before_first_start() {
        let script = build_win_install_script(
            &install_params(
                PathBuf::from("/x"),
                PathBuf::from(r"C:\app\polaris-helper.exe"),
            ),
            "WTOKEN",
        );
        // ① IU 启动权：逐字钉死 sdset 行——IU 的第二段 ACE 恰好只有 RP（只授 start，不授
        // stop/改配置）；改任何一段（尤其给 IU 加权）都该过 review 而不是悄悄过。
        assert!(
            script.contains(
                "& $sc sdset PolarisHelper \"D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)(A;;RP;;;IU)S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)\""
            ),
            "缺 sdset 授 IU SERVICE_START（W20 恢复腿的硬前提）"
        );
        // ② 失败恢复 + ③ 三道退出码守卫（sdset/failure/start）+ ④ 次序：配置先于首启。
        let sdset_at = script.find("sdset PolarisHelper").expect("缺 sdset");
        let failure_line = "& $sc failure PolarisHelper reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null";
        let failure_at = script.find(failure_line).expect("缺 sc failure 自愈配置");
        let start_at = script
            .find("& $sc start PolarisHelper")
            .expect("缺 sc start");
        assert!(
            sdset_at < failure_at && failure_at < start_at,
            "自愈配置必须在首次 start 之前（首启即被覆盖）"
        );
        assert_eq!(
            script.matches("if ($LASTEXITCODE -ne 0)").count(),
            3,
            "sdset/failure/start 三步必须各带退出码守卫（EAP=Stop 拦不住外部程序非零退出）"
        );
    }

    /// W24：覆盖升级不是“删旧 → 祈祷新服务能起”。旧 helper/token/SCM 快照必须先于第一处
    /// 破坏性写入；只有新服务首启成功才删备份；catch 必须恢复两份文件和原服务运行态。
    #[test]
    fn win_helper_upgrade_is_transactional_and_keeps_recovery_copies_on_failure() {
        let script = build_win_install_script(
            &install_params(
                PathBuf::from("/x"),
                PathBuf::from(r"C:\app\polaris-helper.exe"),
            ),
            "WTOKEN",
        );
        let snapshot = script
            .find("$oldService = Get-CimInstance")
            .expect("缺旧 SCM 快照");
        let backup_helper = script
            .find("Copy-Item -LiteralPath $helperDst -Destination $helperBackup")
            .expect("缺旧 helper 备份");
        let backup_token = script
            .find("Copy-Item -LiteralPath $tokenFile -Destination $tokenBackup")
            .expect("缺旧 token 备份");
        let transaction = script.find("try {\n").expect("缺升级事务边界");
        let destructive = script
            .find("Remove-Item -Force -Path $tokenFile")
            .expect("缺 token 替换腿");
        let first_start = script
            .find("& $sc start PolarisHelper")
            .expect("缺新服务首启");
        let commit = first_start
            + script[first_start..]
                .find("Remove-Item -Force -Path $helperBackup,$tokenBackup")
                .expect("缺成功 commit 清备份");
        let rollback = script.find("} catch {\n").expect("缺失败回滚腿");

        assert!(
            snapshot < backup_helper
                && backup_helper < backup_token
                && backup_token < transaction
                && transaction < destructive,
            "SCM/helper/token 快照必须完整发生在首个破坏性写入之前"
        );
        assert!(
            first_start < commit && commit < rollback,
            "首启成功后才能 commit"
        );
        for needle in [
            "Copy-Item -LiteralPath $helperBackup -Destination $helperDst -Force",
            "Copy-Item -LiteralPath $tokenBackup -Destination $tokenFile -Force",
            "New-Service -Name PolarisHelper -BinaryPathName $oldBinPath",
            "if ($oldWasRunning) { & $sc start PolarisHelper",
            "失败时保留 .rollback",
        ] {
            assert!(script[rollback..].contains(needle), "回滚腿缺：{needle}");
        }
    }

    #[test]
    fn uninstall_scripts_remove_service_and_files() {
        let mac = build_mac_uninstall_script(&InstallPaths::mac());
        assert!(mac.contains("launchctl bootout system \"$PLIST\""));
        assert!(mac.contains("rm -rf '/Library/Application Support/Polaris'"));
        let lin = build_linux_uninstall_script(&InstallPaths::linux());
        assert!(lin.contains("systemctl disable --now polaris-helper.service"));
        assert!(lin.contains("rm -rf '/usr/local/lib/polaris' '/var/lib/polaris' '/run/polaris'"));
        let win = build_win_uninstall_script();
        assert!(win.contains("& $sc delete PolarisHelper"));
        assert!(win.contains("& $sc stop PolarisHelper"));
        // 同 install 的病根牙（最强形）：单引号包任何 $ 引用的调用即病；本脚本 EAP=
        // SilentlyContinue，病发时静默什么都不卸——「卸载点了没反应」的隐性形态。
        assert!(
            !win.contains("& '$"),
            "win 卸载脚本出现单引号包 $ 引用的调用——静默不卸的必死形态"
        );
        assert!(win.contains(r"Remove-Item -Recurse -Force -Path 'C:\ProgramData\Polaris'"));
    }

    // ── install()/uninstall() 端到端（提权接线 + 落盘 + 清理）──
    #[test]
    fn install_linux_end_to_end_wires_pkexec_and_writes_script() {
        let src = PathBuf::from("/app/polaris-helper");
        let (m, _tok_dir) = install_manager(Platform::Linux, &src);
        let script_dir = tempfile::tempdir().unwrap();
        let params = install_params(script_dir.path().to_path_buf(), src);
        let (exec, calls, content) = capturing((String::new(), 0));
        let outcome = m.install(&params, &exec).unwrap();
        assert_eq!(outcome, EscalationOutcome::Success);
        // 提权走 pkexec，argv[0..2] = pkexec /bin/bash <script>。
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "/usr/bin/pkexec");
        assert_eq!(calls[0][1], "/bin/bash");
        assert!(
            calls[0][2].starts_with(&script_dir.path().to_string_lossy().into_owned()),
            "脚本落 script_dir"
        );
        // executor 读到的脚本内容 = build_linux_install_script（落盘真发生）。
        let written = content.lock().unwrap().clone().expect("脚本应已落盘");
        assert!(written.contains("systemctl enable --now polaris-helper.service"));
        // 清理：脚本文件已删（finally unlink）。
        assert!(!Path::new(&calls[0][2]).exists(), "脚本执行后须清理");
    }

    #[test]
    fn install_missing_binary_errors() {
        // 源二进制不在 sysops.exists → HelperBinaryMissing（不落脚本、不提权）。
        let dir = tempfile::tempdir().unwrap();
        let m = HelperManager::new(
            Platform::Linux,
            dir.path().join("token"),
            Box::new(MockSysOps::default()),
        );
        let script_dir = tempfile::tempdir().unwrap();
        let params = install_params(
            script_dir.path().to_path_buf(),
            PathBuf::from("/app/absent"),
        );
        let (exec, calls, _) = capturing((String::new(), 0));
        let err = m.install(&params, &exec).unwrap_err();
        assert!(matches!(err, ManagerError::HelperBinaryMissing(_)));
        assert!(calls.lock().unwrap().is_empty(), "缺二进制不应提权");
    }

    #[test]
    fn install_mac_selects_osascript() {
        let src = PathBuf::from("/app/helper");
        let (m, _d) = install_manager(Platform::Mac, &src);
        let script_dir = tempfile::tempdir().unwrap();
        let params = install_params(script_dir.path().to_path_buf(), src);
        let (exec, calls, _) = capturing((String::new(), 0));
        m.install(&params, &exec).unwrap();
        assert_eq!(calls.lock().unwrap()[0][0], "/usr/bin/osascript");
    }

    #[test]
    fn install_win_selects_uac() {
        let src = PathBuf::from(r"C:\app\helper.exe");
        let (m, _d) = install_manager(Platform::Win, &src);
        let script_dir = tempfile::tempdir().unwrap();
        let params = install_params(script_dir.path().to_path_buf(), src);
        let (exec, calls, _) = capturing((String::new(), 0));
        m.install(&params, &exec).unwrap();
        assert_eq!(calls.lock().unwrap()[0][0], "powershell.exe");
    }

    #[test]
    fn install_user_cancel_maps_to_cancelled() {
        // pkexec 126 = 取消 → Cancelled（非 Err，取消是正常流程）。
        let src = PathBuf::from("/app/helper");
        let (m, _d) = install_manager(Platform::Linux, &src);
        let script_dir = tempfile::tempdir().unwrap();
        let params = install_params(script_dir.path().to_path_buf(), src);
        let (exec, _, _) = capturing(("".into(), 126));
        assert_eq!(
            m.install(&params, &exec).unwrap(),
            EscalationOutcome::Cancelled
        );
    }

    #[test]
    fn uninstall_clears_token_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        crate::token::write_token_content(&token_path, "TOK").unwrap();
        assert!(token_path.exists());
        let m = HelperManager::new(
            Platform::Linux,
            token_path.clone(),
            Box::new(MockSysOps::default()),
        );
        let script_dir = tempfile::tempdir().unwrap();
        let (exec, calls, _) = capturing((String::new(), 0));
        let outcome = m.uninstall(script_dir.path(), &exec).unwrap();
        assert_eq!(outcome, EscalationOutcome::Success);
        assert!(!token_path.exists(), "卸载成功须清 app 侧 token");
        assert_eq!(calls.lock().unwrap()[0][0], "/usr/bin/pkexec");
    }

    #[test]
    fn uninstall_cancel_keeps_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        crate::token::write_token_content(&token_path, "TOK").unwrap();
        let m = HelperManager::new(
            Platform::Linux,
            token_path.clone(),
            Box::new(MockSysOps::default()),
        );
        let script_dir = tempfile::tempdir().unwrap();
        let (exec, _, _) = capturing(("".into(), 126));
        assert_eq!(
            m.uninstall(script_dir.path(), &exec).unwrap(),
            EscalationOutcome::Cancelled
        );
        assert!(token_path.exists(), "取消不应清 token（helper 仍在）");
    }

    #[test]
    fn pipe_self_uninstall_only_for_win() {
        // 非 win 恒 false（无 uninstall 命令），不触发 client。
        let m = manager(Platform::Linux, MockSysOps::default());
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Linux, "");
        assert!(!m.pipe_self_uninstall(&client));
    }

    #[test]
    fn pipe_self_uninstall_win_ok_response() {
        let m = manager(Platform::Win, MockSysOps::default());
        // helper 回 OK（win uninstall 命令，W11）→ true。
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::with_response(
                b"OK uninstalling\n".to_vec(),
            )])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Win, "TOK");
        assert!(m.pipe_self_uninstall(&client));
    }

    #[test]
    fn wait_until_ready_returns_ready_when_proto_ok() {
        let paths = InstallPaths::mac();
        let mut sysops = MockSysOps::default();
        sysops.exists_paths.insert(paths.binary.clone());
        sysops
            .exists_paths
            .insert(paths.descriptor.clone().expect("mac/linux 有描述符文件"));
        let (m, _dir) = manager_with_token(Platform::Mac, sysops);
        let connector = MockConnector {
            streams: Arc::new(Mutex::new(vec![MockStream::with_response(
                b"OK pong uid=0 v9\n".to_vec(),
            )])),
        };
        let client = HelperClient::new(Box::new(connector), Platform::Mac, "TOK");
        let status = m.wait_until_ready(&client, 1, Duration::from_millis(1));
        assert!(status.ready);
    }

    /// Windows 提权脚本必须带 UTF-8 BOM —— 见 [`script_bytes`] 的文档。
    ///
    /// 判据落在**字节**上（不是「代码里有没有那个常量」）：BOM 掉了、或被误加到 unix 腿上，都必红。
    #[test]
    fn windows_script_carries_a_utf8_bom_and_unix_never_does() {
        const SRC: &str = "$ErrorActionPreference = 'Stop'\n# 复制 helper.exe 失败\n";
        let win = script_bytes(SRC, true);
        assert_eq!(&win[..3], &[0xEF, 0xBB, 0xBF], "Windows 腿丢了 BOM");
        assert_eq!(
            &win[3..],
            SRC.as_bytes(),
            "BOM 之后必须逐字节等于原文（别顺手改了编码）"
        );

        let nix = script_bytes(SRC, false);
        assert_eq!(
            nix,
            SRC.as_bytes(),
            "unix 腿不得有 BOM（会顶掉 shebang 的首字节）"
        );

        // 正向对照：两条腿确实不同形，否则上面两条可能同时被一个「恒不加 BOM」的实现满足。
        assert_ne!(win, nix);
    }

    /// 两个 `write_secure_script` 分支各自传对了 `windows` 实参 —— 纯函数测不到接线。
    #[test]
    fn both_write_legs_pass_the_right_platform_flag() {
        let src = include_str!("manager.rs");
        // 切「锚点之后的第一个顶层 `#[cfg(test)]`」。**不能切第一个** —— 本文件第一个 `#[cfg(test)]`
        // 在 :29（远早于 `write_secure_script`），切它会把待验函数整个丢掉，门以 panic 收场。
        let at = src
            .find("fn write_secure_script(")
            .expect("write_secure_script 消失，门失去判据");
        let end = src[at..]
            .find("\n#[cfg(test)]\n")
            .map_or(src.len(), |i| at + i);
        let body = &src[at..end];
        // 切点自检：判据区域里若混进本测试自己，下面三条会被自己写的字面量喂饱。
        assert!(
            !body.contains("both_write_legs_pass_the_right_platform_flag"),
            "切点错了：判据区域包含本测试自身"
        );
        assert!(
            body.contains("script_bytes(content, false)"),
            "unix 腿没走 script_bytes(.., false)"
        );
        assert!(
            body.contains("script_bytes(content, true)"),
            "windows 腿没走 script_bytes(.., true) —— BOM 不会被写出去"
        );
        assert!(
            !body.contains("f.write_all(content.as_bytes())"),
            "还有分支在绕过 script_bytes 直接写原文"
        );
    }
}
