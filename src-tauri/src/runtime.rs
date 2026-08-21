//! 运行时层：把 17 个纯逻辑/trait crate 装配为持有真实 I/O 实现的运行时实例。
//!
//! 架构（见系统设计 §B.2 / §H）：各 domain crate 是纯逻辑 + trait 抽象（`ConfigFs` / `SingBoxSpawner`
//! / `ManagementApi` / `HttpClient` / `SysOps` 等），src-tauri 在此注入真实实现——
//! `tokio` runtime、`std::fs`、真 TCP/UDS socket、真 HTTP 客户端——构成可被 `#[tauri::command]`
//! 直接调用的运行时管理器。
//!
//! 模块：
//! - [`config`]：`ConfigManager`（`store::ConfigStore` + `StdFs` + currentConfig 缓存，Polaris ConfigManager 等价）。
//! - [`proxy`]：`ProxyRuntime`（sing-box 进程编排：spawn + 系统代理 + clash_api 管理；Polaris ProxyManager 等价）。
//! - [`stats`]：`StatsRelay`（stats-engine 订阅注册表 + 流 relay 到 Tauri event）。
//! - [`geo_seed`]：随包内置 geo `.srs` 播种到运行时 rules 目录（上游 `seedBuiltinRuleSets` 等价）。
//! - [`helper`]：`HelperRuntime`（helper-client + 平台 helper 装配）。
//! - [`mesh`]：`MeshRuntime`（warp 注册 / tailscale 登录 / exit route，mesh crate 装配）。
//!
//! 注：各运行时的访问器（dir / path / platform / config 等）为后续 actor 批次（crash-recovery /
//! stats-worker / mesh-warp）预留注入点；当前仅 command 层用到子集，`dead_code` 全模块放行。

#![allow(dead_code)]

pub mod auto_switch;
pub mod config;
pub mod core_paths;
pub mod core_promote;
pub mod core_swap;
pub mod core_update_scheduler;
pub mod core_validation;
pub mod geo_seed;
pub mod helper;
pub mod http;
pub mod management_api;
pub mod mesh;
/// pending `modified`（全维）与测速 dirty（5 维）两条判据的单点定义 + 包含关系不变式。
pub mod node_fingerprints;
pub mod proxy;
pub mod rule_resource_scheduler;
pub mod speedtest;
/// 单节点测速的 **CONNECT 隧道**探针（建隧道 + 隧道内两次 GET；`commands/speedtest.rs` 的传输面）。
pub mod speedtest_tunnel;
pub mod startup_tasks;
pub mod stats;
pub mod subscription_scheduler;
pub mod tailscale_login_core;
pub mod tailscale_status;
pub mod uninstall;
pub mod unlock;
pub mod update_install;
pub mod update_popup;
pub mod updater;
pub mod win_console;
#[cfg(windows)]
pub(crate) mod windows_process;
pub mod x25519;

use std::sync::Arc;

use crate::runtime::{
    config::ConfigManager, helper::HelperRuntime, http::HttpRuntime, mesh::MeshRuntime,
    proxy::ProxyRuntime, stats::StatsRelay, unlock::UnlockRuntime, updater::UpdaterRuntime,
};

/// 聚合所有运行时实例的根（注入 Tauri `State`）。
///
/// 各字段用 `Arc<...>` 以便后台任务（stats relay / 事件广播）克隆持有、跨 `tokio::spawn` 边界。
/// 单实例（`manage` 一次），command 经 `State<'_, AppRuntime>` 取引用。
pub struct AppRuntime {
    pub config: Arc<ConfigManager>,
    pub proxy: Arc<ProxyRuntime>,
    pub stats: Arc<StatsRelay>,
    pub helper: Arc<HelperRuntime>,
    pub mesh: Arc<MeshRuntime>,
    /// 更新运行时（App 自更新 + 内核更新 + mini 弹窗会话）。
    pub updater: Arc<UpdaterRuntime>,
    /// 传输层单点：**全 App 唯一**的真实 HTTP/TLS 客户端（见 `runtime/http.rs`）。
    /// 订阅拉取 / 内核下载 / 解锁检测 / WARP 全部经它注入既有窄 trait。
    pub http: Arc<HttpRuntime>,
    /// 解锁检测编排（run/get/快照/事件/出口 pin/TTL/归属 bracket；见 `runtime/unlock.rs`）。
    pub unlock: Arc<UnlockRuntime>,
}

impl AppRuntime {
    /// 装配全部运行时（main 启动期调用一次）。
    ///
    /// 路径锚 `<app_config_dir>/polaris/`（Tauri `path::app_config_dir`，对齐 上游 `app.getPath('userData')`）。
    ///
    /// # Errors
    ///
    /// 传输层 client 初始化失败（TLS 后端异常）。**报错退出优于带残缺网络栈硬跑**：
    /// 无 HTTP client 则订阅/更新/解锁/WARP 全不可用，早失败比运行期到处报「未接线」清晰。
    pub fn new(config_dir: std::path::PathBuf) -> Result<Self, String> {
        let config = Arc::new(ConfigManager::new(config_dir.clone()));
        let helper = Arc::new(HelperRuntime::new(config_dir.clone()));
        // 生产装配：注入 helper → mesh 出口路由 op 启用（真三平台 route 手术）。测试路径用 `MeshRuntime::new`（禁用 op）。
        let mesh = Arc::new(MeshRuntime::new_with_helper(
            config_dir.clone(),
            helper.clone(),
        ));
        let stats = Arc::new(StatsRelay::default());
        let updater = Arc::new(UpdaterRuntime::new(config_dir.clone()));
        let http = Arc::new(HttpRuntime::new()?);
        let unlock = Arc::new(UnlockRuntime::new(http.clone()));
        // 系统代理清理收口器（维度7 #8）：生产装配真实控制器（本机平台命令 + 真实 FS marker）。
        // marker 路径 = `<userData>/system-proxy.marker.json`（对齐 上游 `SystemProxyBase.getMarkerPath`）。
        // 无 marker（fresh start）时 `ensure_cleared` 门控 1 即返、零系统调用 → 挂每个失败腿都安全。
        let proxy_marker_path = config_dir.join(polaris_system_integration::PROXY_MARKER_FILENAME);
        let proxy_clearer: Box<dyn crate::runtime::proxy::SystemProxyClearer> =
            Box::new(polaris_system_integration::production_proxy_controller(
                proxy_marker_path.to_string_lossy().into_owned(),
            ));
        let proxy = Arc::new(ProxyRuntime::new(
            config.clone(),
            helper.clone(),
            mesh.clone(),
            proxy_clearer,
            // C11：竞速 sidecar 的 DoH 上游走同一个 `HttpRuntime`（workspace 唯一真实 HTTP/TLS 客户端）。
            http.clone(),
        ));
        Ok(Self {
            config,
            proxy,
            stats,
            helper,
            mesh,
            updater,
            http,
            unlock,
        })
    }

    /// 配置运行时（命令层便捷访问）。
    #[must_use]
    pub fn config(&self) -> &ConfigManager {
        &self.config
    }

    /// 代理运行时。
    #[must_use]
    pub fn proxy(&self) -> &ProxyRuntime {
        &self.proxy
    }

    /// stats 运行时。
    #[must_use]
    pub fn stats(&self) -> &StatsRelay {
        &self.stats
    }

    /// helper 运行时。
    #[must_use]
    pub fn helper(&self) -> &HelperRuntime {
        &self.helper
    }

    /// mesh 运行时。
    #[must_use]
    pub fn mesh(&self) -> &MeshRuntime {
        &self.mesh
    }

    /// 更新运行时。
    #[must_use]
    pub fn updater(&self) -> &UpdaterRuntime {
        &self.updater
    }

    /// 传输层单点（真实 HTTP client）。
    #[must_use]
    pub fn http(&self) -> &Arc<HttpRuntime> {
        &self.http
    }

    /// 解锁检测编排运行时。
    #[must_use]
    pub fn unlock(&self) -> &UnlockRuntime {
        &self.unlock
    }
}

/// 测试用临时目录（唯一路径 + Drop 清理）。
///
/// **刻意不引 `tempfile`**（简约阶梯第 5 级：stdlib 先于依赖）：本用途只需「唯一目录 + Drop 清理」，
/// stdlib 足够。收在 `runtime` 根供各子模块共用，避免 `runtime/updater.rs` 那份被逐个复制
/// （已有 3 处需要沙箱目录：`updater` / `core_paths` / `core_swap`）。
/// Drop 保证清理是**硬要求**：本机 `/tmp` 是 tmpfs（12G），残留会累积到 Bash 全崩。
#[cfg(test)]
pub(crate) struct TmpDir(std::path::PathBuf);

#[cfg(test)]
impl TmpDir {
    pub(crate) fn new() -> Self {
        // 唯一性：pid + 进程内单调计数器（并发测试也不撞）。
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("polaris-rt-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("建临时测试目录");
        Self(p)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TmpDir {
    fn drop(&mut self) {
        // 尽力清理；失败不 panic（Drop 里 panic 会掩盖真实的测试失败）。
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
