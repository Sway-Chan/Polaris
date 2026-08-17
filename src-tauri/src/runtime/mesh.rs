//! mesh 运行时：`polaris-mesh` 装配（WARP / Tailscale / exit-route）。
//!
//! Polaris 锚点：
//! - `main/services/WarpService.ts` → `polaris_mesh::warp_http::WarpService`（匿名设备注册 → WG 草稿）
//! - `main/services/tailscale-state.ts` → `polaris_mesh::tailscale_state`（TS 节点 state 目录管理）
//! - `MeshExitRouteManager` → `polaris_mesh::exit_route`（mesh 出口路由接管 / 让位）
//!
//! 纯逻辑纪律：mesh crate 的 HTTP/FS/keypair 经 trait 抽象（`UnlockHttp` / `TailscaleStateFs` /
//! `ExitRouteOp`），本层注入真实实现。注册 WARP 需真实 HTTP + keypair 生成（Curve25519），
//! 属系统交互批次；本层提供 tailscale state（纯文件操作，注入 [`StdTailscaleFs`]）+ 命令入口。

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use polaris_mesh::tailscale_state::{state_exists, tailscale_state_dir, TailscaleStateFs};
use polaris_mesh::warp::{
    enqueue_pending_deregister, plan_deregister_drain, DeregisterResult, DrainAction,
    DrainPlanItem, PendingDeregisterEntry,
};
use polaris_mesh::warp_http::{
    WarpHttp, WarpHttpRequest, WarpHttpResponse, WarpKeypair, WarpLog, WarpService,
};
use polaris_mesh::{ExitRouteCancel, ExitRouteLog, ExitRouteOp, MeshExitRouteManager, Platform};
use tokio::sync::Mutex as AsyncMutex;

use crate::runtime::helper::HelperRuntime;
use crate::runtime::http::HttpRuntime;
use crate::runtime::tailscale_login_core::{
    AppHandleEmitter, LoginCoreRegistry, StartLoginOutcome,
};
use crate::runtime::tailscale_status::{TailscaleStatusEvent, TailscaleStatusSnapshot};
use crate::runtime::x25519;
use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::server_config::ServerConfig;
use tauri::AppHandle;

/// 基于 `std::fs` 的 [`TailscaleStateFs`] 实现（应用层注入）。
/// Polaris 用 `fs.readdirSync(dir)` 直接读盘；失败安全返 None（对齐 Polaris catch → false）。
struct StdTailscaleFs;

impl TailscaleStateFs for StdTailscaleFs {
    fn read_dir_names(&self, dir: &Path) -> Option<Vec<String>> {
        std::fs::read_dir(dir)
            .ok()?
            .map(|res| {
                res.ok()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
            })
            .collect()
    }
}

/// WARP 待注销队列 drain 周期（启动先跑一次，之后按此间隔）。Polaris 无显式常量（按事件 + 定时驱动），
/// 取 1h 折中：孤儿设备清理不必激进（`WARP_DEREGISTER_MAX_AGE_MS`=7 天护栏 + 单次 `MAX_PER_DRAIN`=10 限流
/// 已避免 hammer CF），过密只徒增 CF 1020 风险。真机可再校准。
const WARP_DRAIN_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// 组网登录期出口让位判定输入（上游 `shared/mesh-login-fallback.ts` `MeshLoginFallbackInput` 1:1 镜像）。
///
/// 全部字段与「default=proxy-selector→未连上 TS 出口」这一死锁形态一一对应（见 [`mesh_login_fallback_should_engage`]）。
/// 纯输入、无 I/O：便于单测 + 单一真值防漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshLoginFallbackInput {
    /// 开关：`meshLoginFallbackDirect !== false`（默认开）。
    pub fallback_enabled: bool,
    /// 当前是否 direct 代理模式（default 本就 direct，不适用让位）。
    pub proxy_mode_direct: bool,
    /// 选中出口是否已「回退直连」（`meshSelectedExitFallsBackToDirect`：off-mesh / 仅子网段组网节点）。
    pub selected_exit_falls_back_direct: bool,
    /// 选中出口是否为 Tailscale 协议。
    pub selected_is_tailscale: bool,
    /// 选中 TS 是否配置了 authKey（静态凭据，无交互登录死锁）。
    pub selected_has_auth_key: bool,
    /// 选中 TS 隧道是否已就绪（STATUS backendState=Running）。
    pub selected_tunnel_ready: bool,
}

/// 是否应让默认路由让位直连（引导期）。上游 `meshLoginFallbackShouldEngage`（1:1 移植）。
///
/// 场景（缺陷 1）：选中出口为账号制 Tailscale 且承载全隧道时，proxy-selector.default = 该 TS endpoint。
/// 隧道尚未 Running（未登录/未授权/netmap 未同步）时，浏览器授权页与引导期控制平面流量被导向这个「尚未
/// 连上的出口」→ 授权页打不开 → 授权永不完成 → 引导链死锁。治法：就绪前把默认路由临时热切 direct（零重启），
/// Running 后切回。本谓词判「配置层是否符合让位形态」；就绪与否（tunnel_ready）由 reconcile 按 backendState 决策。
#[must_use]
pub fn mesh_login_fallback_should_engage(i: &MeshLoginFallbackInput) -> bool {
    i.fallback_enabled
        && !i.proxy_mode_direct
        && !i.selected_exit_falls_back_direct
        && i.selected_is_tailscale
        && !i.selected_has_auth_key
        && !i.selected_tunnel_ready
}

/// mesh 运行时（`State`-managed，单实例）。
pub struct MeshRuntime {
    /// 配置根（`<app_config_dir>/polaris/`）。tailscale state 子目录由 crate 自算 `<root>/tailscale/<id>`。
    config_dir: PathBuf,
    /// warp 待注销队列持久化路径。
    warp_queue_path: PathBuf,
    /// warp 队列文件读改写串行化锁（enqueue 同步命令线程 + drain 异步任务共享同一队列文件，
    /// 防交错丢更新）。锁只护「读→改→写」临界段，**绝不跨 await 持有**（drain 的网络调用在锁外）。
    warp_queue_lock: Mutex<()>,
    /// Tailscale 瞬态登录核生命周期注册表（与 `ProxyRuntime` 常驻代理核隔离）。
    login_registry: LoginCoreRegistry,
    /// C5 mesh 出口路由托管状态机（`MeshExitRouteManager`，1:1 移植自 上游 `MeshExitRouteManager`）。
    /// async `Mutex`：其 `reconcile`/`clear`/`reassert` 是 `&mut self` async（macOS apply 轮询接口可达
    /// 跨 await），须异步锁串行化独占访问。**OS 路由真操作经 [`HelperExitRouteOp`]**：`MeshRuntime::new`
    /// （测试/未接线默认）注入 `enabled=false` 的诚实 no-op op（`installed` 恒 None，绝不碰宿主网络）；
    /// `MeshRuntime::new_with_helper`（生产 `AppRuntime::new`）注入 `enabled=true` op（真三平台 route 手术，真机门）。
    exit_route: AsyncMutex<MeshExitRouteManager<HelperExitRouteOp, LogExitRouteLog>>,
    /// 出口路由在飞作业的取消令牌（**锁外**句柄，与状态机内那份是同一个 `Arc`）。
    ///
    /// 存在的唯一理由是「取消必须在拿到锁之前就能发出」：macOS 反查轮询持锁最长 18s，若取消信号
    /// 也要先拿锁才发得出去，它就得排在那 18s 后面 = 什么都没解决。见 [`polaris_mesh::ExitRouteCancel`]。
    exit_route_cancel: Arc<ExitRouteCancel>,
    /// 出口路由 op 的调用计数（供接线单测断言「生命周期腿真触达状态机」；生产侧仅原子自增，可忽略）。
    exit_route_stats: Arc<ExitRouteOpStats>,
    /// A3：Tailscale STATUS 流末帧缓存（各在册 TS 节点的解码事件）。
    ///
    /// `None` = 尚无帧（核未起 / 起后尚未收到首帧 / 停核已清）。relay（`proxy.rs::spawn_tailscale_status_relay`）
    /// 每收一帧全量端点快照即整体替换（`update_ts_status`），停核清空（`clear_ts_status`）；
    /// `tailscale_get_status` 命令读它（配合核 running 态给出 `connected`）。
    /// `RwLock`：relay 单写、命令多读，无跨 await 持锁。
    ts_status: RwLock<Option<Vec<TailscaleStatusEvent>>>,
}

impl MeshRuntime {
    /// 测试/未接线默认构造：出口路由 op **禁用**（`enabled=false`，helper=None）——诚实 no-op，绝不 shell
    /// 任何 `ip`/`route` 命令、绝不碰宿主网络。生产装配走 [`Self::new_with_helper`]（注入 helper + 启用真手术）。
    #[must_use]
    pub fn new(config_dir: PathBuf) -> Self {
        let stats = Arc::new(ExitRouteOpStats::default());
        let op = HelperExitRouteOp {
            helper: None,
            platform: current_platform(),
            enabled: false,
            stats: stats.clone(),
        };
        Self::from_parts(config_dir, op, stats)
    }

    /// 生产构造（`AppRuntime::new`）：注入就绪 helper → 出口路由 op **启用**（`enabled=true`）。
    /// 此后 `exit_route_reconcile`/`exit_route_clear` 会真做三平台 route 手术（mac/win 经 helper `route -ifscope`、
    /// Linux app 自身 `ip route` 独立表 7732）——属**真机门**（本机开发/单测路径永不经此构造）。
    #[must_use]
    pub fn new_with_helper(config_dir: PathBuf, helper: Arc<HelperRuntime>) -> Self {
        let stats = Arc::new(ExitRouteOpStats::default());
        let op = HelperExitRouteOp {
            helper: Some(helper),
            platform: current_platform(),
            enabled: true,
            stats: stats.clone(),
        };
        Self::from_parts(config_dir, op, stats)
    }

    /// 两构造共用装配（仅出口路由 op 不同）。
    fn from_parts(
        config_dir: PathBuf,
        op: HelperExitRouteOp,
        exit_route_stats: Arc<ExitRouteOpStats>,
    ) -> Self {
        let warp_queue_path = config_dir.join("warp-deregister-queue.json");
        let manager = MeshExitRouteManager::new(op, LogExitRouteLog, current_platform());
        // 取消令牌由状态机自持，此处取同一个 Arc 的锁外句柄（不是第二份状态）。
        let exit_route_cancel = manager.cancel_handle();
        Self {
            config_dir,
            warp_queue_path,
            warp_queue_lock: Mutex::new(()),
            login_registry: LoginCoreRegistry::production(),
            exit_route: AsyncMutex::new(manager),
            exit_route_cancel,
            exit_route_stats,
            ts_status: RwLock::new(None),
        }
    }

    /// A3：relay 收到一帧全量 TS 端点快照 → 整体替换末帧缓存（非增量：每帧即全量）。
    pub fn update_ts_status(&self, statuses: Vec<TailscaleStatusEvent>) {
        if let Ok(mut g) = self.ts_status.write() {
            *g = Some(statuses);
        }
    }

    /// A3：停核 → 清 TS 状态末帧缓存（陈旧 live 数据不再供 `tailscale_get_status`；核未跑即诚实空）。
    pub fn clear_ts_status(&self) {
        if let Ok(mut g) = self.ts_status.write() {
            *g = None;
        }
    }

    /// A3：`TAILSCALE_GET_STATUS` 拉缓存末帧 + 新鲜度。`connected` 由调用方传入（= 主核是否在运行，
    /// 即状态流是否 live）——缓存本身不含 running 态，二者在命令层合成。缓存空（无帧/已清）→ `statuses: []`。
    #[must_use]
    pub fn tailscale_status_snapshot(&self, connected: bool) -> TailscaleStatusSnapshot {
        let statuses = self
            .ts_status
            .read()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        TailscaleStatusSnapshot {
            connected,
            statuses,
        }
    }

    /// A4：选中出口 STATUS 末帧 backendState（读缓存）。上游 `selectedExitBackendState`。
    ///
    /// `expired` → 视作 `"NeedsLogin"`（key 过期即便 backendState 仍 Running 也须重新交互登录，否则过期后
    /// 走死出口黑洞）。无该端点帧（核未起 / 未选中 TS / 首帧未到）→ `None`。登录期出口让位对账据此三态决策。
    #[must_use]
    pub fn selected_exit_backend_state(&self, selected_id: &str) -> Option<String> {
        let guard = self.ts_status.read().ok()?;
        let statuses = guard.as_ref()?;
        let ev = statuses.iter().find(|e| e.server_id == selected_id)?;
        if ev.expired {
            return Some("NeedsLogin".to_string());
        }
        Some(ev.backend_state.clone())
    }

    /// **廉价存在性探问**：末帧缓存里是否有任何在册 TS 端点（`None` / 空 vec → false）。
    ///
    /// 存在的理由是**每帧调用方的开销**：`proxy.rs::reconcile_ts_exit_block` 由 STATUS relay 每帧
    /// （~1/s）驱动，其判定需要深拷贝整份配置（含 200 节点级 `servers` 数组）+ 反序列化。而无任何 TS
    /// 帧时该判定的结果**恒为「无告警」**（`derive_ts_exit_warning` 在 `logged_in=false` 时提前返回），
    /// 故用这个只读锁 + 一次 `is_empty` 把绝大多数用户（不用 Tailscale）的那份常驻开销整个挡掉。
    /// **只跳过工作、绝不改变结论**（等价性由 `exit_block_is_none_when_status_cache_empty` 钉住）。
    #[must_use]
    pub fn has_ts_status(&self) -> bool {
        self.ts_status
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|v| !v.is_empty()))
            .unwrap_or(false)
    }

    /// item6：某在册 TS 节点的 STATUS 末帧（供选中出口无效直判读 `peers`/`logged_in`）。
    /// 无帧（核未起/未收首帧/已清）/ 未在册 → None。`RwLock` 读，clone 出帧不持锁跨用。
    #[must_use]
    pub fn ts_status_event(&self, server_id: &str) -> Option<TailscaleStatusEvent> {
        let guard = self.ts_status.read().ok()?;
        guard
            .as_ref()?
            .iter()
            .find(|e| e.server_id == server_id)
            .cloned()
    }

    /// 配置根（tailscale state 目录由 [`tailscale_state_dir`] 自算，勿再 join "tailscale"）。
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// 某节点 tailscale state 目录（`<config_dir>/tailscale/<server_id>`）。
    #[must_use]
    pub fn tailscale_state_dir(&self, server_id: &str) -> PathBuf {
        tailscale_state_dir(&self.config_dir, server_id)
    }

    /// 批量查 TS 节点 state 目录存在性（上游 `tailscale:stateExists`，纯文件存在性判定）。
    pub fn tailscale_state_exists(
        &self,
        server_ids: &[String],
    ) -> std::collections::HashMap<String, bool> {
        let fs = StdTailscaleFs;
        let mut out = std::collections::HashMap::new();
        for id in server_ids {
            out.insert(id.clone(), state_exists(&fs, &self.config_dir, id));
        }
        out
    }

    /// 退出某节点 TS 登录（上游 `tailscale:logout`）：清 state 目录（best-effort，不存在不报错）。
    pub fn tailscale_logout(&self, server_id: &str) -> std::io::Result<()> {
        let dir = self.tailscale_state_dir(server_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// warp 待注销队列路径（供待注销队列 actor 持久化）。
    #[must_use]
    pub fn warp_queue_path(&self) -> &Path {
        &self.warp_queue_path
    }

    /// 读待注销队列（缺失/损坏 → 空，best-effort 不 panic）。**须在持 `warp_queue_lock` 时调**。
    fn load_warp_queue(&self) -> Vec<PendingDeregisterEntry> {
        match std::fs::read_to_string(&self.warp_queue_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// 原子写待注销队列（临时文件 + rename）。best-effort：失败仅 warn，不阻断上游删节点。
    /// **须在持 `warp_queue_lock` 时调**。
    fn save_warp_queue(&self, queue: &[PendingDeregisterEntry]) {
        let text = match serde_json::to_string(queue) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("warp 待注销队列序列化失败: {e}");
                return;
            }
        };
        let tmp = self.warp_queue_path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text.as_bytes()) {
            log::warn!("warp 待注销队列写临时文件失败: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.warp_queue_path) {
            log::warn!("warp 待注销队列 rename 失败: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// WARP 节点删除时把远端自删凭据入待注销队列（防孤儿设备计费）。上游 `enqueuePendingDeregister`。
    /// 落盘后由 drain 循环（[`Self::spawn_warp_drain`]）在启动 + 定时 tick 时消费。队列护栏（去最旧超上限）
    /// 与「注销/丢弃/重试」分类判定全在 crate 纯逻辑（`warp.rs`），本层只做锁 + 文件 I/O 装配。
    pub fn enqueue_warp_deregister(&self, device_id: &str, token: &str) {
        if device_id.is_empty() || token.is_empty() {
            return; // 无凭据无从注销。
        }
        let _guard = self
            .warp_queue_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queue = self.load_warp_queue();
        let entry = PendingDeregisterEntry {
            device_id: device_id.to_string(),
            token: token.to_string(),
            enqueued_at: now_millis(),
        };
        let (next, dropped) = enqueue_pending_deregister(&queue, entry);
        for d in &dropped {
            log::warn!(
                "warp 待注销队列超上限，丢弃最旧 device={}…",
                id_prefix(&d.device_id)
            );
        }
        self.save_warp_queue(&next);
    }

    /// drain 一遍队列：超龄条目直接出队（不调网络）；在龄条目调 crate `WarpService::unregister`
    /// （真 CF DELETE），按返回 Done/Drop 出队、Retry 留队。**读→改→写**两段各在锁内、网络调用在锁外，
    /// 出队按精确条目匹配（reload 后 retain），故与并发 `enqueue_warp_deregister` 不丢新入队条目。
    pub async fn drain_warp_deregister_once(&self, http: &Arc<HttpRuntime>) {
        let now = now_millis();
        // ① 锁内取快照 + 算计划（纯逻辑，crate）。
        let (plan, _deferred) = {
            let _guard = self
                .warp_queue_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let snapshot = self.load_warp_queue();
            if snapshot.is_empty() {
                return;
            }
            plan_deregister_drain(&snapshot, now)
        };
        // ② 锁外跑网络（unregister 是 crate 纯编排 + 真 HTTP；drain 用占位种子——unregister 不碰 keypair）。
        let svc = warp_service(http.clone(), [0u8; 32]);
        let mut eligible_results: Vec<DeregisterResult> = Vec::new();
        for item in &plan {
            if item.action == DrainAction::Eligible {
                eligible_results.push(
                    svc.unregister(&item.entry.device_id, &item.entry.token)
                        .await,
                );
            }
        }
        let to_remove = plan_removals(&plan, &eligible_results);
        if to_remove.is_empty() {
            return;
        }
        // ③ 锁内 reload + 精确出队 + 回写（reload 保住网络期间的并发新入队）。
        let _guard = self
            .warp_queue_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.load_warp_queue();
        let next = retain_unresolved(current, &to_remove);
        self.save_warp_queue(&next);
    }

    /// 启动期 drain 一次（清上次退出遗留）+ 定时 drain。经 `tauri::async_runtime::spawn` 常驻后台任务。
    /// **装配点**：`main.rs` setup 内 `AppRuntime::new` 之后、`manage` 之前调
    /// `app_runtime.mesh.clone().spawn_warp_drain(app_runtime.http.clone());`（见交接说明）。
    pub fn spawn_warp_drain(self: Arc<Self>, http: Arc<HttpRuntime>) {
        tauri::async_runtime::spawn(async move {
            // 启动即 drain（消化上次会话遗留的孤儿设备）。
            self.drain_warp_deregister_once(&http).await;
            let mut ticker = tokio::time::interval(WARP_DRAIN_INTERVAL);
            ticker.tick().await; // 首 tick 立即返回，跳过（启动已 drain）。
            loop {
                ticker.tick().await;
                self.drain_warp_deregister_once(&http).await;
            }
        });
    }

    /// 起某 TS 节点的瞬态登录核（上游 `tailscale:login`）：spawn 独立 sing-box，订阅它自己的管理 API
    /// STATUS 流，把帧里的 `authURL` 发成登录 URL 事件、`backendState=Running` 当登录成功并收核。
    ///
    /// `is_running`/`running_config`/`primary_api_port` 由命令层从 `ProxyRuntime` 取：前两者供双写守卫
    /// （该 endpoint 是否已在运行主核），后者供瞬态核 api 端口避开主核已占的那个。
    /// 返 [`StartLoginOutcome`]，命令层折成前端 `{ started, reason?, authUrl? }`。
    pub async fn start_tailscale_login(
        &self,
        app: AppHandle,
        server: &ServerConfig,
        is_running: bool,
        running_config: Option<&UserConfig>,
        primary_api_port: u16,
    ) -> StartLoginOutcome {
        let emitter = Arc::new(AppHandleEmitter { app });
        self.login_registry
            .start_login(
                server,
                &self.config_dir,
                is_running,
                running_config,
                primary_api_port,
                emitter,
            )
            .await
    }

    /// 取消某 TS 节点在飞的瞬态登录核（上游 `tailscale:loginCancel`）。幂等：无在飞核也返 ok。
    pub fn cancel_tailscale_login(&self, server_id: &str) -> bool {
        self.login_registry.cancel_login(server_id)
    }

    // ── C5 mesh 出口路由生命周期腿（ProxyRuntime 核生命周期接线）───────────────────────────────
    //
    // 契约 special #37「绝不抢 sing-box 路由」的让位语义**在 crate 内建**（`plan_mesh_exit_route` 仅当
    // 选中的全局出口 = TS System + 承载全隧道时才装单条 ifscope default，其余一律 None=让位）——本层只做
    // 生命周期接线，不改让位判定。**OS 路由真操作经 [`HelperExitRouteOp`]，已全链接线**：生产构造
    // （[`Self::new_with_helper`]）下 mac/win 经 root/SYSTEM helper `route -ifscope`、Linux 经自身
    // `ip rule/route` 独立表 7732 —— 真手术、真机门；测试构造（[`Self::new`]，`enabled=false`）诚实 no-op。

    /// 起核前快照 utun 基线（macOS：时序 diff 锚点；其它平台 no-op）。ProxyRuntime 在 spawn 核**前**调用
    /// （须早于核创建 TS 内核接口）。
    pub async fn exit_route_snapshot_baseline(&self) {
        // 新一轮起核 = 上一轮的在飞反查（macOS 最长 18s）已彻底作废：先抢占再排队，否则新 start
        // 的整条起核流程要跟着那 18s 一起等（旧腿的世代守卫只挡「再开一轮」，挡不住已在轮询的那轮）。
        self.exit_route_cancel.cancel();
        self.exit_route.lock().await.snapshot_baseline().await;
    }

    /// 对齐出口路由到目标配置（起核就绪 / 切节点 / 切模式后调用，fire-and-forget，绝不抛）。
    /// 生产（`enabled`）下真做 route 手术（真机门）；测试/未接线（`enabled=false`）下诚实 no-op（`installed` 恒 None）。
    ///
    /// **不 cancel、只带凭据**：本腿与在飞那轮属**同一个核会话**，目标接口也是同一张 —— 打断它再从头
    /// 轮询一遍，总时长不变、只多一次 churn。故这里只做「排队期间是否被停核/复位抢占」的判定
    /// （凭据须在**排队之前**快照，见 [`polaris_mesh::ExitRouteCancel::token`]）。
    ///
    /// 同一份 `token` 还要**传进状态机**：拿锁后的这次判定只覆盖「排队期间」，而锁内还有
    /// `clear_inner` 的真实 await —— 那段窗口里发生的 cancel 只有靠这份凭据一路传到 `apply` 才认得出
    /// （状态机内部二次快照 = 把取消吞掉，见 `ExitRouteCancel::token` 文档）。
    pub async fn exit_route_reconcile(&self, config: &UserConfig, enable_ipv6: bool) {
        let token = self.exit_route_cancel.token();
        let mut mgr = self.exit_route.lock().await;
        if self.exit_route_cancel.is_cancelled(token) {
            log::debug!("mesh 出口路由 reconcile：排队期间已被停核/复位抢占 → 放弃本轮");
            return;
        }
        let outcome = mgr.reconcile(config, enable_ipv6, token).await;
        if outcome.changed {
            log::debug!("mesh 出口路由 reconcile：状态机判定有变更");
        }
    }

    /// 停核 / teardown：清理已装出口路由（未装成 / 禁用 op 下 `installed` 恒 None → clear_inner 早退 = 纯 no-op）。
    ///
    /// **先 cancel 再排队**：这正是「点停止最长卡 18s」的修法 —— 取消信号必须走锁外通道发出去，
    /// 在飞的 macOS 反查轮询在一个周期（1.5s）内收手，本方法随即拿到锁。
    pub async fn exit_route_clear(&self) {
        self.exit_route_cancel.cancel();
        self.exit_route.lock().await.clear().await;
    }

    /// TS 出口 re-advertise 恢复腿的重申（上游 `reassert`，R3）。
    ///
    /// **生产调用点**：`proxy.rs::ts_exit_recover_once`（R2 出口恢复腿，由 STATUS 帧的
    /// blocked→none 翻转对账驱动）。修的是 crate 侧 [`MeshExitRouteManager::reassert`] 文档所述的两个真缺口
    /// （installed 为空 = resolveIface 18s 轮询超时过 / macOS iface 已消失），**不 churn 已存路由**。
    ///
    /// **排队期间的抢占判定**（凭据在拿锁前快照）：调用方 `ts_exit_recover_once` 在调本方法前刚比过
    /// 世代，但那之后还要排 `exit_route` 这把锁 —— 恰在排队期间停核的话，`clear` 先跑完，本腿随后
    /// 醒来会看到 `installed=None` 而去**给一个已停的核重装出口路由**（Linux 下反查直接返逻辑名，
    /// 一装一个准）。世代守卫够不着这个窗口（它在锁外判、锁在它之后拿），故这里再判一次凭据。
    pub async fn exit_route_reassert(&self, config: &UserConfig, enable_ipv6: bool) {
        let token = self.exit_route_cancel.token();
        let mut mgr = self.exit_route.lock().await;
        if self.exit_route_cancel.is_cancelled(token) {
            log::debug!("mesh 出口路由 reassert：排队期间已被停核/复位抢占 → 放弃本轮");
            return;
        }
        mgr.reassert(config, enable_ipv6, token).await;
    }

    /// 崩溃 / 非正常拆除的同步内存态复位（上游 `resetState`）：内核接口随进程消失、其路由已自动失效，
    /// 故不发删命令，仅清残留 `installed`（防下次 reconcile 误判已装 → 黑洞）。
    pub async fn exit_route_reset_state(&self) {
        // 崩溃拆除同样是「在飞那轮已作废」：不抢占的话，这条同步复位要排在 18s 轮询后面，
        // 而崩溃恢复腿正等着它把内存态清干净才敢重起核。
        self.exit_route_cancel.cancel();
        self.exit_route.lock().await.reset_state();
    }

    /// 出口路由当前内存态（**仅测试**观测：接线单测断言占位 op 恒不装路由）。
    #[cfg(test)]
    async fn exit_route_installed(&self) -> Option<polaris_mesh::InstalledRoute> {
        self.exit_route.lock().await.installed().cloned()
    }

    /// **仅测试**：占住 `exit_route` 锁直到被通知放手，给 `runtime::proxy::stop_inner` 的换代守卫
    /// 造一个**确定性** await 窗口。
    ///
    /// `stop_inner` 的拆除段里 [`exit_route_clear`](Self::exit_route_clear) 是第一个必然让出执行权的
    /// 点（`lock().await` 拿不到就一定挂起）⇒ 本方法持着锁时停核腿**不可能**越过它，于是测试可以在
    /// 那之后不慌不忙地制造一次换代，再放锁看它是否让位。没有这个窗口就只能靠 sleep 赌时序 ——
    /// 那种测试的绿是没有信息量的。
    ///
    /// 用「占位任务 + 两个 [`Notify`](tokio::sync::Notify)」而不是把 `MutexGuard` 返回给调用方：
    /// 后者要在签名里写出 `MeshExitRouteManager<HelperExitRouteOp, LogExitRouteLog>`，等于为了一条测试
    /// 把两个私有装配类型提成 `pub(crate)`。
    #[cfg(test)]
    pub(crate) async fn occupy_exit_route_lock_for_test(
        &self,
        acquired: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        let _guard = self.exit_route.lock().await;
        acquired.notify_one();
        release.notified().await;
    }
}

// ── WARP 服务装配（注入真实 HTTP + keypair + 日志，供 warp_register/warp_apply_license 命令）─────
//
// `polaris_mesh::WarpService<H,K,L>` 是纯逻辑编排（register/applyLicense/unregister），把网络/密钥/日志
// 抽象成 trait。本层注入三个真实实现：
//   H = [`HttpWarpAdapter`]：转发到 [`HttpRuntime`] 的 `WarpHttp` 实现（reqwest+rustls，见 runtime/http.rs）。
//   K = [`SeededWarpKeypair`]：ring CSPRNG 出种子 + RFC 7748 X25519 出公钥（见 runtime/x25519.rs）。
//   L = [`LogWarpLog`]：转发到 `log` crate。

/// 把 `Arc<HttpRuntime>` 适配成 `WarpHttp`（`HttpRuntime` 已实现 `WarpHttp`，此处只做 Arc 转发）。
///
/// 为何要 newtype：`WarpService` 按值持有 `H: WarpHttp`，而命令层握的是 `Arc<HttpRuntime>`；
/// `impl WarpHttp for Arc<HttpRuntime>` 触孤儿规则（trait 与 `Arc` 皆非本 crate）。故 newtype 绕开。
pub struct HttpWarpAdapter(Arc<HttpRuntime>);

#[async_trait]
impl WarpHttp for HttpWarpAdapter {
    async fn json_request(&self, req: &WarpHttpRequest) -> Result<String, String> {
        self.0.json_request(req).await
    }
    async fn status_request(&self, req: &WarpHttpRequest) -> Result<WarpHttpResponse, String> {
        self.0.status_request(req).await
    }
}

/// 由固定 32 字节种子产出 WARP 的 WG keypair（base64 私钥 = 裸种子，公钥 = X25519(种子, 基点)）。
///
/// 种子在命令层用 CSPRNG 预生成（[`generate_warp_seed`]，可失败 → 结构化 error），本类型的
/// `generate_keypair` 遂是**确定性、不可失败**（X25519 标量乘无失败态），满足 `WarpKeypair` 的无错契约。
/// 对齐 上游 `WarpService.generateKeyPair`：存储私钥为**未裁剪**的裸种子（node PKCS8 末 32 字节同款）。
pub struct SeededWarpKeypair {
    seed: [u8; 32],
}

impl WarpKeypair for SeededWarpKeypair {
    fn generate_keypair(&self) -> (String, String) {
        let public = x25519::x25519_base(&self.seed);
        (base64_encode(&self.seed), base64_encode(&public))
    }
}

/// WARP 日志：转发到 `log` crate（对齐 上游 `LogManager` 的 info/warn/error 落盘最小面）。
pub struct LogWarpLog;

impl WarpLog for LogWarpLog {
    fn log(&self, level: &str, message: &str) {
        match level {
            "error" => log::error!("[WarpService] {message}"),
            "warn" => log::warn!("[WarpService] {message}"),
            _ => log::info!("[WarpService] {message}"),
        }
    }
}

/// 用 CSPRNG 生成 32 字节 WARP 私钥种子。
///
/// **不新增依赖**：走 rustls（本仓直接依赖）暴露的 ring `SecureRandom`（`crypto::ring::default_provider`
/// 的 `secure_random` 字段即 ring `SystemRandom`）。失败（OS 熵源不可用）返结构化 Err，命令层转 error code
/// —— 对齐 node `crypto.generateKeyPairSync` 熵源失败即抛（绝不静默返弱/零密钥）。
///
/// # Errors
/// 系统 CSPRNG 不可用（`GetRandomFailed`）。
pub fn generate_warp_seed() -> Result<[u8; 32], String> {
    let mut seed = [0u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut seed)
        .map_err(|_| "系统随机源不可用，无法生成 WARP 密钥".to_string())?;
    Ok(seed)
}

/// 装配一个 WARP 服务（注入真实 HTTP + 给定种子的 keypair + 日志）。
///
/// register 路径传 [`generate_warp_seed`] 出的真种子；applyLicense 路径不触碰 keypair（`WarpService::apply_license`
/// 从不调 `generate_keypair`），故可传占位种子 `[0u8; 32]`（永不被用到）。
#[must_use]
pub fn warp_service(
    http: Arc<HttpRuntime>,
    seed: [u8; 32],
) -> WarpService<HttpWarpAdapter, SeededWarpKeypair, LogWarpLog> {
    WarpService::new(
        HttpWarpAdapter(http),
        SeededWarpKeypair { seed },
        LogWarpLog,
    )
}

/// 标准 base64 编码（带 padding）。WARP 的 32 字节私钥/公钥 → 44 字符 base64。
/// 单一用途 32→44，`base64` crate 已在图中但避免升级为直接依赖（禁引新依赖）→ 最小实现。
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 当前 unix 毫秒（对齐 上游 `Date.now()`）。时钟异常 → 0（不 panic）。
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// deviceId 日志前缀（绝不打全 id/token）。
fn id_prefix(device_id: &str) -> String {
    device_id.chars().take(8).collect()
}

/// drain 计划 + 各 Eligible 条目的注销结果 → 应出队条目集（纯逻辑，便于变异测试）。
///
/// - `Expire`：超龄放弃，出队；
/// - `Eligible` 且 `Done`/`Drop`：注销成功或凭据死，出队；
/// - `Eligible` 且 `Retry`：留队（不入移除集）。
///
/// `eligible_results` 与 plan 中 `Eligible` 条目**按序一一对应**（drain 顺序遍历产出）。缺项兜底 `Retry`（留队，
/// 宁可多试一次不误删）。
fn plan_removals(
    plan: &[DrainPlanItem],
    eligible_results: &[DeregisterResult],
) -> Vec<PendingDeregisterEntry> {
    let mut remove = Vec::new();
    let mut ri = 0usize;
    for item in plan {
        match item.action {
            DrainAction::Expire => remove.push(item.entry.clone()),
            DrainAction::Eligible => {
                let result = eligible_results
                    .get(ri)
                    .copied()
                    .unwrap_or(DeregisterResult::Retry);
                ri += 1;
                if matches!(result, DeregisterResult::Done | DeregisterResult::Drop) {
                    remove.push(item.entry.clone());
                }
            }
        }
    }
    remove
}

/// 从当前队列剔除已解决（出队）条目，保留其余（含 Retry + 网络期间的并发新入队）。精确条目匹配
/// （`PendingDeregisterEntry` 全字段 `Eq`，含 `enqueued_at`）→ 不误删同 device 的另一次入队。
fn retain_unresolved(
    current: Vec<PendingDeregisterEntry>,
    removed: &[PendingDeregisterEntry],
) -> Vec<PendingDeregisterEntry> {
    current
        .into_iter()
        .filter(|e| !removed.contains(e))
        .collect()
}

// ── C5 出口路由生产 OS 手术（三平台 route 装/卸 + macOS utun 反查）──────────────────────────────

/// 出口路由 op 的调用计数（接线单测用；生产侧仅原子自增，成本可忽略）。
#[derive(Default)]
struct ExitRouteOpStats {
    /// `run_route`（装/删路由）被调次数。
    route_calls: AtomicU64,
    /// `find_tailnet_iface`（反查内核接口）被调次数。
    iface_lookups: AtomicU64,
}

/// macOS 反查 TS 内核接口的轮询预算（核连上 tailnet 后 utun 才出现，起核后数秒）。上游 `resolveIface`：12×1.5s≈18s。
const MACOS_RESOLVE_ATTEMPTS: u32 = 12;
const MACOS_RESOLVE_DELAY: Duration = Duration::from_millis(1500);
/// Linux 出口路由独立表 + 规则优先级（绝不碰 main 表 → 不抢 sing-box 主 TUN/子网路由）。Polaris runRoute linux：7732。
const LINUX_EXIT_TABLE: &str = "7732";
const LINUX_EXIT_RULE_PRIORITY: &str = "7732";

/// 生产 [`ExitRouteOp`]：mesh 出口路由真 OS 手术（1:1 移植 上游 `MeshExitRouteManager.runRoute` /
/// `listUtuns` / `probeMacosTailnetIface`）。
///
/// 平台分派（**真机门**：真改宿主路由/查接口）：
/// - **macOS**：`ifconfig` 反查 TS utun（起核后新增 utun 时序 diff + tailnet 100.64/10 地址）→ helper(root)
///   `route add/del -ifscope`。utun 名动态，故轮询等待接口出现（[`MACOS_RESOLVE_ATTEMPTS`]）。
/// - **Linux**：app 自身 CAP_NET_ADMIN → 独立表 [`LINUX_EXIT_TABLE`] + `oif` 规则 `ip route/rule`
///   （**绝不碰 main 表**；helper 协议蓄意无 Linux route 命令 → 不经 helper）。sing-box 只装 tailnet/accept
///   子网路由、**不装 exit_node 的 0/0 出口路由**（真机实证）→ 须本 op 补 0/0（否则绑接口 dialer 拨公网 unreachable）。
/// - **Windows**：`MeshExitRouteManager` 入口已 no-op（禁 System），本 op 不到达该分派。
///
/// **`enabled` 闸门**：`false`（`MeshRuntime::new` 测试/未接线默认，`helper=None`）→ 三方法诚实 no-op
/// （`run_route`→false / `find_tailnet_iface`→None / `list_utuns`→空），**绝不 spawn 任何进程**（本机 Linux
/// 开发/单测安全，杜绝改宿主网络）。`true`（`MeshRuntime::new_with_helper` 生产）→ 真 OS 手术。
struct HelperExitRouteOp {
    /// mac/win route 手术经此 helper（root/SYSTEM `route -ifscope`）。Linux 不用（直接 `ip`）；禁用态 = None。
    helper: Option<Arc<HelperRuntime>>,
    platform: Platform,
    /// 生产接线闸门（见类型注释）。
    enabled: bool,
    stats: Arc<ExitRouteOpStats>,
}

#[async_trait]
impl ExitRouteOp for HelperExitRouteOp {
    async fn run_route(&self, op: &str, iface: &str, cidrs: &[String]) -> bool {
        self.stats.route_calls.fetch_add(1, Ordering::SeqCst);
        if !self.enabled {
            log::debug!("出口路由 OS 操作未接线(禁用闸门)：route-{op} iface={iface} → no-op");
            return false; // 诚实：不假装 OS 路由已装（管理器不标 installed）
        }
        match self.platform {
            // Linux/其它类 unix：app 自身 CAP_NET_ADMIN，独立表 + oif 规则。
            //
            // **返回值由 `ip rule add` 的退出码决定，不再无条件 true**：`run_ip_command` 吞掉全部错误
            // （`ip` 不在 PATH / 无 CAP_NET_ADMIN / 内核无 policy routing 全落同一条 best-effort 路径），
            // 恒 true 会让状态机把「一条都没装上」记成 `installed` —— 后果有二：① 用户以为 System 出口
            // 已生效，实则公网 unreachable；② clear 时对**不存在**的路由发 del（噪音，且掩盖真实失败）。
            // 门取 `rule add` 而非「全部命令」：规则装不上 ⇒ 表 7732 永不被查中 ⇒ 里面的路由一条都不生效
            // （真·全败）；而单条 `route replace` 失败（如 v6 cidr 在关掉 IPv6 的机器上）不代表整腿失败，
            // 此时仍标 installed 才能让 clear 把已装的那部分收回去（否则泄漏）。
            // add 腿首条即 `rule add`（见 [`linux_route_argv`] 的构造顺序，由单测钉死）。
            // del 腿维持 best-effort true：clear 是幂等收尾，返 false 只会多一条 warn，不改变状态。
            //
            // 判定与执行分离（[`run_linux_route_seq`]）：真 `ip` 调用是真机门（本机绝不 spawn），
            // 而「哪条是门、失败后跳不跳、del 腿返什么」是可测的纯编排 —— 单测注入假 runner 覆盖。
            Platform::Linux | Platform::Other => {
                run_linux_route_seq(op, linux_route_argv(op, iface, cidrs), |argv| async move {
                    run_ip_command(&argv).await
                })
                .await
            }
            // mac/win：经 root/SYSTEM helper（`route -ifscope`）。res.ok 决定是否标 installed（诚实）。
            Platform::Mac | Platform::Win => match &self.helper {
                Some(h) => h.route_op(op, iface, cidrs),
                None => {
                    log::warn!("出口路由:helper 不可用,无法 route-{op}(System+exit 出网将不通)");
                    false
                }
            },
        }
    }

    async fn list_utuns(&self) -> HashSet<String> {
        if !self.enabled || self.platform != Platform::Mac {
            return HashSet::new(); // 仅 macOS 有动态 utun 需快照；禁用态一律空
        }
        match run_command_stdout("ifconfig", &["-l"]).await {
            Some(out) => parse_utun_list(&out),
            None => HashSet::new(),
        }
    }

    async fn find_tailnet_iface(
        &self,
        logical_name: &str,
        baseline: Option<&HashSet<String>>,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Option<String> {
        self.stats.iface_lookups.fetch_add(1, Ordering::SeqCst);
        if !self.enabled {
            return None; // 禁用闸门 → 管理器 apply 短路，不装路由
        }
        if self.platform != Platform::Mac {
            // Linux/其它：内核接口固定逻辑名（polaris-ts）。不轮询 ⇒ 天然满足取消契约。
            return Some(logical_name.to_string());
        }
        // macOS：核连上 tailnet 后 TS utun 才出现（起核后数秒）→ 轮询等待。
        // 轮询编排（含取消判据）抽成 [`poll_for_tailnet_iface`]：真 `ifconfig` 是真机门（本机绝不
        // spawn），而「取消后几轮内退出」是可测的纯编排 —— 单测注入假 probe 覆盖。
        poll_for_tailnet_iface(
            MACOS_RESOLVE_ATTEMPTS,
            MACOS_RESOLVE_DELAY,
            cancelled,
            || async {
                let out = run_command_stdout("ifconfig", &[]).await?;
                pick_tailnet_iface(&parse_ifconfig_ifaces(&out), baseline)
            },
        )
        .await
    }
}

/// macOS TS 内核接口反查的**轮询编排**（注入式：`probe` 单次探测、`cancelled` 取消判据）。
///
/// 每一轮的顺序是「查取消 → 探测 → sleep」：取消判据放在**探测之前**，故 `cancel()` 之后最多再
/// 睡一个 `delay` 就退出（收手窗口 ≤ 一个周期）。这正是 [`ExitRouteCancel`] 要求实现方守的契约 ——
/// 不守就是「点停止最长卡 `attempts × delay`（生产 12×1.5s≈18s）」。
///
/// 取消时返回 `None` 与「探测不到」同码：调用方（`polaris_mesh` 的 `apply`）自己再查一次凭据来区分
/// 日志措辞，且两条腿都**不装路由** ⇒ `installed` 保持 `None`，状态自洽。
async fn poll_for_tailnet_iface<F, Fut>(
    attempts: u32,
    delay: Duration,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
    mut probe: F,
) -> Option<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    for _ in 0..attempts {
        if cancelled() {
            log::debug!("出口路由:TS 接口反查被取消(停核/复位/新起核) → 提前退出轮询");
            return None;
        }
        if let Some(found) = probe().await {
            return Some(found);
        }
        tokio::time::sleep(delay).await;
    }
    None
}

/// Linux 出口路由 argv 序列（独立表 [`LINUX_EXIT_TABLE`] + oif 规则，绝不碰 main 表 → 不抢 sing-box）。
/// Polaris runRoute linux 1:1：
/// - add：`rule add oif <iface> table T priority P` + 逐 cidr `route replace <cidr> dev <iface> table T`；
/// - del：逐 cidr `route del <cidr> dev <iface> table T` + `rule del oif <iface> table T priority P`。
///
/// 纯函数（无副作用），供单测/变异；执行由 [`run_ip_command`] 逐条 best-effort 跑（真机门）。
fn linux_route_argv(op: &str, iface: &str, cidrs: &[String]) -> Vec<Vec<String>> {
    let rule = |verb: &str| {
        vec![
            "rule".to_string(),
            verb.to_string(),
            "oif".to_string(),
            iface.to_string(),
            "table".to_string(),
            LINUX_EXIT_TABLE.to_string(),
            "priority".to_string(),
            LINUX_EXIT_RULE_PRIORITY.to_string(),
        ]
    };
    let route = |verb: &str, cidr: &str| {
        vec![
            "route".to_string(),
            verb.to_string(),
            cidr.to_string(),
            "dev".to_string(),
            iface.to_string(),
            "table".to_string(),
            LINUX_EXIT_TABLE.to_string(),
        ]
    };
    let mut cmds = Vec::new();
    if op == "add" {
        cmds.push(rule("add"));
        for c in cidrs {
            cmds.push(route("replace", c));
        }
    } else {
        for c in cidrs {
            cmds.push(route("del", c));
        }
        cmds.push(rule("del"));
    }
    cmds
}

/// `ifconfig -l` 输出 → utun 接口名集合（`^utun\d+$`）。上游 `listUtuns`。纯函数。
fn parse_utun_list(stdout: &str) -> HashSet<String> {
    stdout
        .split_whitespace()
        .filter(|n| is_utun_name(n))
        .map(String::from)
        .collect()
}

/// `utun` + 纯数字后缀（`^utun\d+$`）。
fn is_utun_name(n: &str) -> bool {
    n.strip_prefix("utun")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// `ifconfig`（全量）输出 → 每张 utun 接口的 inet(v4) 地址表（保序）。上游 `probeMacosTailnetIface` 解析段。纯函数。
fn parse_ifconfig_ifaces(stdout: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in stdout.lines() {
        if let Some(name) = utun_header_name(line) {
            out.push((name, Vec::new()));
        } else if let Some((_, ips)) = out.last_mut() {
            if let Some(ip) = inet_v4_addr(line) {
                ips.push(ip);
            }
        }
    }
    out
}

/// 行首（无缩进）`utunN:` → 接口名；否则 None。明细行有缩进 → 不匹配。
fn utun_header_name(line: &str) -> Option<String> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let name = line.split(':').next()?;
    is_utun_name(name).then(|| name.to_string())
}

/// 缩进的 `inet <v4> ...` 明细行 → v4 地址（点分四段数字）；否则 None。仅 IPv4（`inet ` 带空格避开 `inet6`）。
fn inet_v4_addr(line: &str) -> Option<String> {
    let addr = line
        .trim_start()
        .strip_prefix("inet ")?
        .split_whitespace()
        .next()?;
    let ok = addr.split('.').count() == 4
        && addr
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then(|| addr.to_string())
}

/// tailnet 地址判定（100.64.0.0/10 → 100.64.x - 100.127.x）。上游 `isTailnet`。纯函数。
fn is_tailnet_addr(ip: &str) -> bool {
    let mut it = ip.split('.');
    if it.next() != Some("100") {
        return false;
    }
    it.next()
        .and_then(|s| s.parse::<u32>().ok())
        .is_some_and(|n| (64..=127).contains(&n))
}

/// 从 ifconfig 解析结果挑 TS 内核接口：优先「起核后新增（不在 baseline）且带 tailnet 地址」的 utun；
/// 兜底全量 utun 里带 tailnet 地址的（无 baseline / baseline 偏差）。上游 `probeMacosTailnetIface` 决策。纯函数。
fn pick_tailnet_iface(
    ifaces: &[(String, Vec<String>)],
    baseline: Option<&HashSet<String>>,
) -> Option<String> {
    let has_tailnet = |ips: &[String]| ips.iter().any(|ip| is_tailnet_addr(ip));
    // 候选：起核后新增（不在 baseline）；无 baseline → 全部。
    if let Some((name, _)) = ifaces
        .iter()
        .filter(|(n, _)| baseline.is_none_or(|b| !b.contains(n)))
        .find(|(_, ips)| has_tailnet(ips))
    {
        return Some(name.clone());
    }
    // 兜底：全量 utun 里找带 tailnet 地址的。
    ifaces
        .iter()
        .find(|(_, ips)| has_tailnet(ips))
        .map(|(n, _)| n.clone())
}

/// Linux 出口路由命令序列的执行编排 + **返回值判定**（`run` 注入 ⇒ 本机可测，绝不 spawn 真 `ip`）。
///
/// 语义（对齐 [`HelperExitRouteOp::run_route`] Linux 分支的注释）：
/// - `op == "add"`：**首条即 `ip rule add`**（[`linux_route_argv`] 的构造顺序，由 `linux_add_argv_starts_with_rule_add` 钉死）。
///   它失败 ⇒ 表 7732 永不被查中 ⇒ 后续 `route replace` 全是白跑 ⇒ 立即 break 并返 `false`（不标 installed）。
///   它成功 ⇒ 后续逐条 best-effort（单条 cidr 失败不否定整腿：仍需标 installed 才能在 clear 时收回已装部分）。
/// - `op != "add"`（del）：全程 best-effort，恒 `true`（clear 是幂等收尾，返 false 只多一条 warn）。
async fn run_linux_route_seq<F, Fut>(op: &str, cmds: Vec<Vec<String>>, run: F) -> bool
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let is_add = op == "add";
    for (idx, argv) in cmds.into_iter().enumerate() {
        let ok = run(argv).await;
        if is_add && idx == 0 && !ok {
            log::warn!(
                "出口路由:Linux `ip rule add` 失败(ip 缺失/无 CAP_NET_ADMIN/内核无策略路由?) → 不标 installed,跳过后续 route 命令"
            );
            return false;
        }
    }
    true
}

/// 运行 `ip <argv>`，**返回是否真的成功**（退出码 0）。Linux 出口路由手术（app CAP_NET_ADMIN）。**真机门**。
///
/// 仍不抛（错误只记日志），但**不再把失败伪装成成功**：调用方据返回值决定是否标 `installed`
/// （见 [`HelperExitRouteOp::run_route`] 的 Linux 分支）。`ip` 不在 PATH（`Err`）与非零退出
/// （无 CAP_NET_ADMIN / 语法错 / 内核不支持）都返 `false`。
async fn run_ip_command(argv: &[String]) -> bool {
    match tokio::process::Command::new("ip").args(argv).output().await {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            log::debug!(
                "出口路由 ip {} → 非零退出({:?})",
                argv.join(" "),
                o.status.code()
            );
            false
        }
        Err(e) => {
            log::debug!("出口路由 ip {} 启动失败: {e}", argv.join(" "));
            false
        }
    }
}

/// 运行命令取 stdout（失败 → None）。macOS `ifconfig` 反查用。**真机门**。
async fn run_command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 出口路由日志：转发到 `log` crate（对齐 [`LogWarpLog`] 的 info/warn/error 最小面）。
struct LogExitRouteLog;
impl ExitRouteLog for LogExitRouteLog {
    fn log(&self, level: &str, message: &str) {
        match level {
            "error" => log::error!("[MeshExitRoute] {message}"),
            "warn" => log::warn!("[MeshExitRoute] {message}"),
            _ => log::info!("[MeshExitRoute] {message}"),
        }
    }
}

/// 本机运行平台 → crate `Platform`（供 [`MeshExitRouteManager`] 运行期平台分派）。
///
/// 用 `cfg!`（布尔宏，**非** `#[cfg]` 属性）→ 三平台编译同一单元、无 per-平台死代码，仅运行值不同：
/// 本机（Linux）编到 `Platform::Linux`（mesh_system_supported=true）；macOS/Win 分支为运行值、非编译门控，
/// 故 exit_route 状态机不含任何 `target_os` 分支 → 无待交叉编译的碰不到分支。
fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(target_os = "windows") {
        Platform::Win
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    }
}

#[cfg(test)]
mod warp_tests {
    use super::*;
    use std::sync::Mutex;

    use polaris_mesh::warp_http::RegisterOptions;

    // ── 组合面门（§K7.1）：mock WarpHttp 注入 + 真 keypair + 真 WarpService → 真解析 WG 草稿 ──────
    //
    // 只 mock 网络；keypair（ring 种子 + X25519）、register body 构造、响应解析、草稿装配全走真实路径。
    // 单测 crate 内部函数不够（那只覆盖 mesh crate）；此处覆盖「命令用的确切装配」。

    /// mock：register 返预设 JSON（并把 register body 捕获到共享 handle）；applyLicense（/account）返预设或 Err。
    /// 捕获用 `Arc<Mutex<..>>`（clone 一份 handle 留在测试里，mock 本体 move 进 WarpService 后仍可读）。
    struct MockWarpHttp {
        register_body: String,
        account_body: Option<String>,
        captured_register_body: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl WarpHttp for MockWarpHttp {
        async fn json_request(&self, req: &WarpHttpRequest) -> Result<String, String> {
            if req.url.contains("/account") {
                self.account_body
                    .clone()
                    .ok_or_else(|| "WARP API 403: error 1020".to_string())
            } else {
                *self.captured_register_body.lock().unwrap() = req.body.clone();
                Ok(self.register_body.clone())
            }
        }
        async fn status_request(&self, _req: &WarpHttpRequest) -> Result<WarpHttpResponse, String> {
            Err("status_request 不应在 register/applyLicense 路径被调".to_string())
        }
    }

    fn canned_register(warp_plus: bool) -> String {
        serde_json::json!({
            "id": "devid-123",
            "token": "secret-token",
            "account": { "id": "acctid", "license": "lic", "warp_plus": warp_plus },
            "config": {
                "client_id": "AAEC",
                "interface": { "addresses": { "v4": "172.16.0.2", "v6": "2606:4700:110::1" } },
                "peers": [{ "public_key": "PEERPUBKEY", "endpoint": { "host": "engage.cloudflareclient.com:2408" } }],
            }
        })
        .to_string()
    }

    /// base64 字符数 → 解码字节数（仅测试断言长度用）。
    fn b64_len(s: &str) -> usize {
        s.bytes().filter(|c| *c != b'=').count() * 6 / 8
    }

    #[tokio::test]
    async fn warp_register_end_to_end_real_keypair_mock_http() {
        // 真种子 → 真 X25519 公钥 → 真 register body → mock CF 响应 → 真解析草稿。
        let seed = generate_warp_seed().expect("CSPRNG 应可用");
        let capture = Arc::new(Mutex::new(None));
        let mock = MockWarpHttp {
            register_body: canned_register(false),
            account_body: None,
            captured_register_body: capture.clone(),
        };
        let svc = WarpService::new(mock, SeededWarpKeypair { seed }, LogWarpLog);
        let draft = svc
            .register(RegisterOptions::default())
            .await
            .expect("mock 注册应产出草稿");

        // 私钥 = 裸种子的 base64（keypair 真喂进去了）。
        assert_eq!(draft.private_key, base64_encode(&seed));
        assert_eq!(b64_len(&draft.private_key), 32, "私钥应为 32 字节");
        // 公钥解析自 mock CF 响应（真解析）。
        assert_eq!(draft.peer_public_key, "PEERPUBKEY");
        assert_eq!(draft.address, "engage.cloudflareclient.com");
        assert_eq!(draft.port, 2408);
        assert_eq!(
            draft.local_address,
            vec!["172.16.0.2/32", "2606:4700:110::1/128"]
        );
        assert_eq!(draft.warp_device.device_id, "devid-123");
        assert_eq!(draft.warp_device.token, "secret-token");
        assert!(!draft.meta.warp_plus);

        // 组合面门加强（keypair 生成门）：register body 里的 "key" == 由种子算出的真 X25519 公钥。
        // 打断 x25519_base → 此断言转红。
        let sent = capture
            .lock()
            .unwrap()
            .clone()
            .expect("register body 应被捕获");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(
            parsed["key"].as_str().unwrap(),
            base64_encode(&x25519::x25519_base(&seed)),
            "register 请求携带的公钥必须由种子经 X25519 导出"
        );
    }

    #[tokio::test]
    async fn warp_apply_license_upgrades_warp_plus_end_to_end() {
        // license 应用门：register 带 licenseKey → applyLicense（mock /account 返 warp_plus:true）→ 草稿 warpPlus=true。
        let seed = generate_warp_seed().expect("CSPRNG 应可用");
        let mock = MockWarpHttp {
            register_body: canned_register(false),
            account_body: Some(
                serde_json::json!({ "warp_plus": true, "license": "newlic" }).to_string(),
            ),
            captured_register_body: Arc::new(Mutex::new(None)),
        };
        let svc = WarpService::new(mock, SeededWarpKeypair { seed }, LogWarpLog);
        let draft = svc
            .register(RegisterOptions {
                license_key: Some("mykey".to_string()),
            })
            .await
            .expect("注册+许可应成功");
        assert!(
            draft.meta.warp_plus,
            "warp_plus 应经 applyLicense 升为 true"
        );
        assert_eq!(draft.meta.license, "newlic");
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // 32 字节 → 44 字符（含 padding）。
        assert_eq!(base64_encode(&[0u8; 32]).len(), 44);
    }

    #[test]
    fn generate_warp_seed_yields_32_bytes_and_varies() {
        let a = generate_warp_seed().expect("CSPRNG 可用");
        let b = generate_warp_seed().expect("CSPRNG 可用");
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "两次种子应不同（CSPRNG）");
    }
}

#[cfg(test)]
mod warp_drain_tests {
    //! C4 装配面门：enqueue 真落盘 + drain 出队/留队映射（网络端由 crate `warp_http` 单测覆盖）。
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-warp-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(id: &str) -> PendingDeregisterEntry {
        PendingDeregisterEntry {
            device_id: id.to_string(),
            token: format!("t-{id}"),
            enqueued_at: 1,
        }
    }

    /// enqueue 必须**真落盘**（server.rs 删 WARP 节点的入队装配）。回归到「只 log 不 enqueue」
    /// （server.rs:541 旧态）→ 队列空 → 此测转红。打断 `save_warp_queue` 亦转红。
    #[test]
    fn enqueue_persists_entry_to_disk() {
        let dir = temp_dir("enqueue");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.enqueue_warp_deregister("dev-1", "tok-1");
        // 全新实例重读磁盘 → 真落盘才在。
        let reloaded = MeshRuntime::new(dir.clone()).load_warp_queue();
        assert_eq!(reloaded.len(), 1, "入队条目须落盘存活");
        assert_eq!(reloaded[0].device_id, "dev-1");
        assert_eq!(reloaded[0].token, "tok-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enqueue_ignores_empty_credentials() {
        let dir = temp_dir("empty");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.enqueue_warp_deregister("", "tok");
        mesh.enqueue_warp_deregister("dev", "");
        assert!(mesh.load_warp_queue().is_empty(), "空凭据不入队");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 队列上限护栏（crate `enqueue_pending_deregister`）经文件层生效：超上限落盘仍封顶、丢最旧。
    #[test]
    fn enqueue_respects_queue_cap_on_disk() {
        use polaris_mesh::warp::WARP_DEREGISTER_MAX_QUEUE;
        let dir = temp_dir("cap");
        let mesh = MeshRuntime::new(dir.clone());
        for i in 0..(WARP_DEREGISTER_MAX_QUEUE + 5) {
            mesh.enqueue_warp_deregister(&format!("dev-{i}"), "tok");
        }
        let q = mesh.load_warp_queue();
        assert_eq!(q.len(), WARP_DEREGISTER_MAX_QUEUE, "落盘队列封顶");
        assert_eq!(
            q.last().unwrap().device_id,
            format!("dev-{}", WARP_DEREGISTER_MAX_QUEUE + 4),
            "最新入队在队尾（最旧被挤掉）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// drain 结果 → 出队集映射：Expire + (Eligible 且 Done/Drop) 出队；Eligible 且 Retry 留队。
    /// 打断（如把 Retry 也算出队）→ 转红。
    #[test]
    fn plan_removals_expire_and_terminal_remove_retry_keeps() {
        let e_expire = entry("expire");
        let e_done = entry("done");
        let e_drop = entry("drop");
        let e_retry = entry("retry");
        let plan = vec![
            DrainPlanItem {
                entry: e_expire.clone(),
                action: DrainAction::Expire,
            },
            DrainPlanItem {
                entry: e_done.clone(),
                action: DrainAction::Eligible,
            },
            DrainPlanItem {
                entry: e_drop.clone(),
                action: DrainAction::Eligible,
            },
            DrainPlanItem {
                entry: e_retry.clone(),
                action: DrainAction::Eligible,
            },
        ];
        // Eligible 顺序：done / drop / retry。
        let results = vec![
            DeregisterResult::Done,
            DeregisterResult::Drop,
            DeregisterResult::Retry,
        ];
        let remove = plan_removals(&plan, &results);
        assert!(remove.contains(&e_expire), "超龄出队");
        assert!(remove.contains(&e_done), "Done 出队");
        assert!(remove.contains(&e_drop), "Drop 出队");
        assert!(!remove.contains(&e_retry), "Retry 必须留队");
        assert_eq!(remove.len(), 3);
    }

    /// reload 后精确出队：只删已解决条目，保留 Retry + 网络期间的并发新入队（防丢更新）。
    #[test]
    fn retain_unresolved_keeps_retry_and_concurrent_enqueue() {
        let resolved = entry("resolved");
        let retry = entry("retry");
        let newly = entry("new"); // drain 网络期间并发入队。
        let current = vec![resolved.clone(), retry.clone(), newly.clone()];
        let next = retain_unresolved(current, std::slice::from_ref(&resolved));
        assert!(!next.contains(&resolved), "已解决条目出队");
        assert!(next.contains(&retry), "Retry 留队");
        assert!(next.contains(&newly), "并发新入队不丢");
        assert_eq!(next.len(), 2);
    }
}

#[cfg(test)]
mod exit_route_wiring_tests {
    //! C5 接线面门：占位 op 诚实 no-op + MeshRuntime 生命周期腿真触达出口路由状态机。
    //! OS 路由真操作（三平台 route 手术）属 helper 批 C6 真机门，本处不覆盖（无真进程/无宿主网络）。
    //! 状态机纯逻辑（reconcile/clear/latest-wins/macOS 防误删）由 `polaris_mesh::exit_route` 单测覆盖。
    use super::*;
    use polaris_config_engine::user_config::server_config::{
        Protocol, ServerConfig, TailscaleSettings,
    };

    /// TS System + 承载全隧道出口 → `plan_mesh_exit_route` 返 Some（须托管路由）。
    fn ts_system_exit_cfg() -> UserConfig {
        let ts = TailscaleSettings {
            reverse_mesh: Some(true),             // system_interface
            exit_node: Some("100.64.0.1".into()), // 承载全隧道
            ..Default::default()
        };
        let server = ServerConfig {
            id: "ts1".into(),
            name: "ts".into(),
            protocol: Protocol::Tailscale,
            tailscale_settings: Some(Box::new(ts)),
            ..Default::default()
        };
        UserConfig {
            servers: vec![server],
            ..Default::default()
        }
    }

    /// 非 mesh 出口（VLESS）→ `plan_mesh_exit_route` 返 None（让位，契约 #37）。
    fn vless_cfg() -> UserConfig {
        UserConfig {
            servers: vec![ServerConfig {
                id: "v1".into(),
                name: "v".into(),
                protocol: Protocol::Vless,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-exitroute-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 禁用（`enabled=false`）op 诚实 no-op：`run_route` 恒 false（绝不报成功、绝不 shell 命令）、反查恒 None、
    /// utun 恒空。这是本机（Linux 开发机）单测**绝不碰宿主网络**的地基。
    /// 打断任一（false→true / None→Some / 空→非空 / 去掉 enabled 闸门致真 shell）→ 本测转红/破坏本机网络。
    #[tokio::test]
    async fn disabled_exit_route_op_is_honest_noop() {
        let op = HelperExitRouteOp {
            helper: None,
            platform: current_platform(),
            enabled: false,
            stats: Arc::new(ExitRouteOpStats::default()),
        };
        assert!(
            !op.run_route("add", "polaris-ts", &["0.0.0.0/0".to_string()])
                .await,
            "禁用 op 绝不报 route 成功（否则假装 OS 路由已装）"
        );
        assert!(
            op.find_tailnet_iface("polaris-ts", None, &|| false)
                .await
                .is_none(),
            "禁用 op 反查内核接口恒 None"
        );
        assert!(op.list_utuns().await.is_empty(), "禁用 op utun 集恒空");
    }

    // ── 取消令牌接线（MED：点停止最长卡 18s）────────────────────────────────────────────
    //
    // 根因不是世代（合法当权的那条腿一样会卡），是 macOS 反查轮询**不可中断**且整条持着
    // `exit_route` 独占锁。两道修法各有一条门：
    // ① 轮询本身要认取消 → `poll_for_tailnet_iface_stops_within_one_round_after_cancel`；
    // ② 抢占方要在**锁外**发得出取消 + 排队方要认「排队期间被抢占」→ 下面两条接线门。

    /// **轮询侧**：取消后必须在**一个周期内**退出，不跑满 12×1.5s 预算。
    ///
    /// `start_paused` 虚拟时钟 ⇒ 12 次 1.5s sleep 不占真实时间，断言的是**轮数**而非墙钟。
    /// 真 `ifconfig` 是真机门，此处注入假 probe（零进程、零宿主网络）。
    ///
    /// **变异锁**：删掉 `poll_for_tailnet_iface` 里的 `if cancelled() { return None }` →
    /// 探测次数变 `MACOS_RESOLVE_ATTEMPTS`（12）→ 转红；把取消判据挪到 `probe().await` **之后** →
    /// 多探一次（3 次）→ 同样转红。
    #[tokio::test(start_paused = true)]
    async fn poll_for_tailnet_iface_stops_within_one_round_after_cancel() {
        let probes = Arc::new(AtomicU64::new(0));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (probes_in, flag_in, flag_chk) = (Arc::clone(&probes), Arc::clone(&flag), flag);
        let cancelled = move || flag_chk.load(Ordering::SeqCst);
        let out = poll_for_tailnet_iface(
            MACOS_RESOLVE_ATTEMPTS,
            MACOS_RESOLVE_DELAY,
            &cancelled,
            move || {
                let (probes, flag) = (Arc::clone(&probes_in), Arc::clone(&flag_in));
                async move {
                    // 第 2 轮探测期间「用户点了停止」（停核腿在锁外 cancel）。
                    if probes.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                        flag.store(true, Ordering::SeqCst);
                    }
                    None
                }
            },
        )
        .await;
        assert!(out.is_none(), "取消 → 不返回接口名（调用方遂不装路由）");
        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "取消后不得再探测：跑满 {MACOS_RESOLVE_ATTEMPTS} 轮就是「点停止最长卡 18s」的原样"
        );
    }

    /// 反证（同一编排的正向腿）：不取消时轮询跑满预算 —— 上面那条的提前退出确由取消引起。
    #[tokio::test(start_paused = true)]
    async fn poll_for_tailnet_iface_uses_full_budget_when_not_cancelled() {
        let probes = Arc::new(AtomicU64::new(0));
        let probes_in = Arc::clone(&probes);
        let out = poll_for_tailnet_iface(MACOS_RESOLVE_ATTEMPTS, MACOS_RESOLVE_DELAY, &|| false, {
            move || {
                let probes = Arc::clone(&probes_in);
                async move {
                    probes.fetch_add(1, Ordering::SeqCst);
                    None
                }
            }
        })
        .await;
        assert!(out.is_none());
        assert_eq!(
            probes.load(Ordering::SeqCst),
            u64::from(MACOS_RESOLVE_ATTEMPTS)
        );
    }

    /// **抢占侧**：三条拆除/换代腿必须在**拿锁之前**把取消发出去。
    ///
    /// 发在锁内等于没发：取消信号自己要先排在那 18s 轮询后面。世代计数变化即「已发出」的可观测证据。
    ///
    /// **变异锁**：删掉 `exit_route_clear` / `exit_route_snapshot_baseline` /
    /// `exit_route_reset_state` 任一条里的 `self.exit_route_cancel.cancel()` → 对应断言转红。
    #[tokio::test]
    async fn teardown_legs_signal_cancel_outside_the_lock() {
        let dir = temp_dir("cancel-signal");
        let mesh = MeshRuntime::new(dir.clone());
        let t0 = mesh.exit_route_cancel.token();
        mesh.exit_route_clear().await;
        let t1 = mesh.exit_route_cancel.token();
        assert_ne!(t1, t0, "停核腿须在锁外先请求取消（否则点停止仍卡 18s）");
        mesh.exit_route_snapshot_baseline().await;
        let t2 = mesh.exit_route_cancel.token();
        assert_ne!(t2, t1, "新一轮起核的基线快照须抢占上一轮在飞反查");
        mesh.exit_route_reset_state().await;
        assert_ne!(
            mesh.exit_route_cancel.token(),
            t2,
            "崩溃复位须抢占在飞反查（复位是重起核的前置）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **排队侧**：凭据在**拿锁之前**快照 ⇒ 排队期间发生的停核能作废这一轮。
    ///
    /// 复现的正是世代守卫够不着的那个窗口：`ts_exit_recover_once` 比完世代才去排 `exit_route` 的锁，
    /// 恰在排队期间用户点了停止 —— 没有本判据的话，这条腿醒来会看到 `installed=None`（clear 刚清过）
    /// 而**给一个已停的核重装出口路由**（Linux 下反查直接返逻辑名，一装一个准）。
    ///
    /// **变异锁**：删掉 `exit_route_reassert`（或 `exit_route_reconcile`）里拿锁后的
    /// `is_cancelled(token)` 早退 → 排队腿会走到 apply→`find_tailnet_iface` ⇒ `iface_lookups` 变 1 → 转红。
    /// 把 `token()` 快照挪到 `lock().await` **之后** → 快照到的已是取消后的新世代 → 同样转红。
    #[tokio::test]
    async fn queued_leg_is_dropped_when_stop_preempts_it_while_waiting_for_the_lock() {
        let dir = temp_dir("cancel-queued");
        let mesh = Arc::new(MeshRuntime::new(dir.clone()));
        // 占住状态机锁 = 模拟「在飞的 macOS 反查正持锁轮询」。
        let guard = mesh.exit_route.lock().await;
        let queued = {
            let m = Arc::clone(&mesh);
            tokio::spawn(async move {
                m.exit_route_reassert(&ts_system_exit_cfg(), false).await;
            })
        };
        // 让排队腿真的跑到 `lock().await`（凭据此时已快照）。
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // 用户点停止：锁外发取消，然后释放锁（= 在飞那轮收手）。
        mesh.exit_route_cancel.cancel();
        drop(guard);
        queued.await.expect("排队腿不得 panic");
        assert_eq!(
            mesh.exit_route_stats.iface_lookups.load(Ordering::SeqCst),
            0,
            "排队期间被停核抢占的腿必须整轮作废：一次反查都不许发起（发起 = 对着已停的核重装路由）"
        );
        assert!(mesh.exit_route_installed().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// reconcile 生命周期腿真触达状态机：TS System 全隧道出口 → plan Some → apply 反查 op
    /// （iface_lookups++），但占位 op 下 `installed` 恒 None（不假装已装）。
    /// 打断 `exit_route_reconcile` 委托 → iface_lookups=0 转红；打断占位 op 使其真装 → installed 非 None 转红。
    ///
    /// Windows 分支断言相反：`mesh_system_supported_on_platform(Win)=false`（Windows 禁 mesh
    /// System 出口，exit_route.rs `reconcile_once` 平台闸门早退）→ 状态机**按契约**不进 apply、
    /// 不触达 op。打断该闸门（Win 上真跑 apply）→ iface_lookups>0 转红。
    #[tokio::test]
    async fn reconcile_reaches_state_machine_but_installs_nothing() {
        let dir = temp_dir("reconcile");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.exit_route_reconcile(&ts_system_exit_cfg(), false)
            .await;
        let lookups = mesh.exit_route_stats.iface_lookups.load(Ordering::SeqCst);
        if cfg!(windows) {
            assert_eq!(
                lookups, 0,
                "Windows 平台闸门：mesh System 出口不支持 → reconcile 早退，不触达 op"
            );
        } else {
            assert!(
                lookups >= 1,
                "reconcile(mesh 出口) 须触达出口路由状态机的 apply→find_tailnet_iface"
            );
        }
        assert!(
            mesh.exit_route_installed().await.is_none(),
            "测试构造（`enabled=false`）：即便 plan Some 也恒不装路由（诚实 no-op，绝不碰宿主网络）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 让位判定（契约 #37）：非 mesh 出口 → plan None → 状态机不进 apply → 不触达 op。
    /// 打断让位（如强行 apply）→ iface_lookups>0 转红。
    #[tokio::test]
    async fn reconcile_yields_for_non_mesh_exit() {
        let dir = temp_dir("yield");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.exit_route_reconcile(&vless_cfg(), false).await;
        assert_eq!(
            mesh.exit_route_stats.iface_lookups.load(Ordering::SeqCst),
            0,
            "非 TS System 出口 plan None（让位）→ 不触达 op"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// clear / snapshot_baseline / reset_state 生命周期腿：占位 op + 无 installed → 纯 no-op，不 panic、不触达 op。
    #[tokio::test]
    async fn clear_baseline_reset_are_noop_without_installed() {
        let dir = temp_dir("clear");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.exit_route_snapshot_baseline().await;
        mesh.exit_route_clear().await;
        mesh.exit_route_reset_state().await;
        assert!(mesh.exit_route_installed().await.is_none());
        // clear 未触达 op（installed 恒 None → clear_inner 早退）。
        assert_eq!(mesh.exit_route_stats.route_calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod exit_route_pure_tests {
    //! C5 出口路由 op 纯逻辑门（argv 构造 / ifconfig 解析 / tailnet 反查）——真 OS 手术是 mac/Linux 真机门，
    //! 但 argv 与解析是纯字符串→数据，可离线单测 + 变异（防真机门代码悄悄写错却无人守）。
    use super::*;

    /// Linux add：先 `rule add`，再逐 cidr `route replace`（独立表 7732）。
    /// 变异：把 add 写成先 route 后 rule、或表号写错、或 replace 写成 add → 断言序列不符 → 转红。
    #[test]
    fn linux_route_argv_add_sequence() {
        let cmds = linux_route_argv(
            "add",
            "polaris-ts",
            &["0.0.0.0/0".to_string(), "::/0".to_string()],
        );
        assert_eq!(cmds.len(), 3, "rule add + 2 条 route replace");
        assert_eq!(
            cmds[0],
            vec![
                "rule",
                "add",
                "oif",
                "polaris-ts",
                "table",
                "7732",
                "priority",
                "7732"
            ]
        );
        assert_eq!(
            cmds[1],
            vec![
                "route",
                "replace",
                "0.0.0.0/0",
                "dev",
                "polaris-ts",
                "table",
                "7732"
            ]
        );
        assert_eq!(
            cmds[2],
            vec![
                "route",
                "replace",
                "::/0",
                "dev",
                "polaris-ts",
                "table",
                "7732"
            ]
        );
    }

    /// Linux del：先逐 cidr `route del`，最后 `rule del`（与 add 逆序，避免规则先删致路由删不掉）。
    /// 变异：把 del 顺序写反（先 rule del）→ 断言 cmds.last() != rule del → 转红。
    #[test]
    fn linux_route_argv_del_sequence() {
        let cmds = linux_route_argv("del", "polaris-ts", &["0.0.0.0/0".to_string()]);
        assert_eq!(cmds.len(), 2, "1 条 route del + rule del");
        assert_eq!(
            cmds[0],
            vec![
                "route",
                "del",
                "0.0.0.0/0",
                "dev",
                "polaris-ts",
                "table",
                "7732"
            ]
        );
        assert_eq!(
            cmds[1],
            vec![
                "rule",
                "del",
                "oif",
                "polaris-ts",
                "table",
                "7732",
                "priority",
                "7732"
            ]
        );
    }

    // ── Linux 出口路由腿的**返回值诚实性**（此前无条件 true = 假成功）────────────────────────
    //
    // 缺陷形态：`run_ip_command` 吞掉全部错误（`ip` 缺失 / 无 CAP_NET_ADMIN / 内核无策略路由），
    // 而 Linux 分支无条件返 true ⇒ 状态机把「一条都没装上」标成 `installed` ⇒ ① 用户以为 System
    // 出口生效、实则公网 unreachable；② clear 时对不存在的路由发 del。下面四条穷举返回值逃逸面。

    /// 假 runner 记录的「真正被执行」的命令序列（失败后是否短路的唯一观测量）。
    type RanCommands = Arc<Mutex<Vec<Vec<String>>>>;

    /// 假 runner：按脚本返回成功/失败，并记录**真正被执行**的命令序列（验证失败后是否短路）。
    fn scripted_runner(
        script: Vec<bool>,
    ) -> (
        impl Fn(Vec<String>) -> std::future::Ready<bool>,
        RanCommands,
    ) {
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        let script = Arc::new(Mutex::new(script.into_iter().collect::<Vec<_>>()));
        let run = move |argv: Vec<String>| {
            let idx = {
                let mut g = sink.lock().unwrap();
                g.push(argv);
                g.len() - 1
            };
            let ok = script.lock().unwrap().get(idx).copied().unwrap_or(true);
            std::future::ready(ok)
        };
        (run, calls)
    }

    /// add 腿的**门**在首条 `ip rule add`：它失败 ⇒ 返 false（不标 installed）且**不再跑**后续 route。
    /// 变异：删掉 `return false` → 返 true 转红；删掉短路（continue 而非 return）→ 执行数 3 转红；
    /// 把门索引从 0 改成别的 → 首条失败被放行 → 返回值转红。
    #[tokio::test]
    async fn linux_add_returns_false_and_short_circuits_when_rule_add_fails() {
        let cmds = linux_route_argv("add", "polaris-ts", &["0.0.0.0/0".into(), "::/0".into()]);
        let (run, calls) = scripted_runner(vec![false]); // 首条 rule add 失败
        let ok = run_linux_route_seq("add", cmds, run).await;
        assert!(
            !ok,
            "rule add 失败仍返 true = 把「一条都没装上」记成 installed"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "规则没装上 ⇒ 表 7732 永不被查中 ⇒ 后续 route replace 是白跑，必须短路"
        );
    }

    /// `rule add` 成功但**单条 cidr** 失败 ⇒ 仍返 true：已装的那部分必须被标 installed，
    /// 否则 clear 收不回去 = 泄漏（典型：关掉 IPv6 的机器上 `::/0` replace 失败）。
    /// 变异：把门改成「全部命令都成功才 true」→ 本测转红。
    #[tokio::test]
    async fn linux_add_tolerates_single_cidr_failure_after_rule_installed() {
        let cmds = linux_route_argv("add", "polaris-ts", &["0.0.0.0/0".into(), "::/0".into()]);
        let (run, calls) = scripted_runner(vec![true, true, false]); // v6 那条失败
        let ok = run_linux_route_seq("add", cmds, run).await;
        assert!(
            ok,
            "规则已装 + v4 路由已装 ⇒ 必须标 installed，否则 clear 收不回已装部分"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            3,
            "rule 成功后逐条跑完，不短路"
        );
    }

    /// 全成功 → true（正向自证：门不是恒 false）。
    #[tokio::test]
    async fn linux_add_returns_true_when_all_succeed() {
        let cmds = linux_route_argv("add", "polaris-ts", &["0.0.0.0/0".into()]);
        let (run, _) = scripted_runner(vec![true, true]);
        assert!(run_linux_route_seq("add", cmds, run).await);
    }

    /// del 腿全程 best-effort：即便**每条**都失败也返 true（clear 是幂等收尾，installed 已被 take）。
    /// 变异：把 del 也纳入门（首条失败即 false）→ 本测转红。
    #[tokio::test]
    async fn linux_del_stays_best_effort_true_even_when_every_command_fails() {
        let cmds = linux_route_argv("del", "polaris-ts", &["0.0.0.0/0".into()]);
        let (run, calls) = scripted_runner(vec![false, false]);
        assert!(run_linux_route_seq("del", cmds, run).await);
        assert_eq!(calls.lock().unwrap().len(), 2, "del 腿逐条跑完，不短路");
    }

    /// 门的**前置不变式**：add 腿首条必须是 `rule add`（索引 0 的门才有意义）。
    /// 变异：把 [`linux_route_argv`] 的 add 顺序改成先 route 后 rule → 本测转红
    /// （而不是让门静默守错命令）。
    #[test]
    fn linux_add_argv_starts_with_rule_add() {
        let cmds = linux_route_argv("add", "polaris-ts", &["0.0.0.0/0".into()]);
        assert_eq!(&cmds[0][..2], &["rule".to_string(), "add".to_string()]);
    }

    /// `ifconfig -l` → 仅 utun\d+ 名（滤掉 lo0/en0/非数字后缀 utunX）。
    #[test]
    fn parse_utun_list_filters_utun_only() {
        let s = "lo0 gif0 stf0 en0 utun0 utun4 utunfoo utun12 bridge0";
        let got = parse_utun_list(s);
        let mut v: Vec<_> = got.into_iter().collect();
        v.sort();
        assert_eq!(v, vec!["utun0", "utun12", "utun4"]);
    }

    /// 全量 ifconfig → 每 utun 的 v4 地址（忽略 inet6、缩进明细归属当前接口头）。
    #[test]
    fn parse_ifconfig_ifaces_groups_v4_by_utun() {
        let s = "\
en0: flags=8863<UP> mtu 1500
\tinet 192.168.1.10 netmask 0xffffff00
utun4: flags=8051<UP> mtu 1400
\tinet6 fe80::1 prefixlen 64
\tinet 100.64.0.7 --> 100.64.0.7 netmask 0xffffffff
utun5: flags=8051<UP> mtu 1280
\tinet 10.0.0.2 --> 10.0.0.2 netmask 0xffffffff
";
        let got = parse_ifconfig_ifaces(s);
        // en0 非 utun → 不入。
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "utun4");
        assert_eq!(got[0].1, vec!["100.64.0.7"]); // inet6 被忽略
        assert_eq!(got[1].0, "utun5");
        assert_eq!(got[1].1, vec!["10.0.0.2"]);
    }

    /// tailnet 100.64.0.0/10 边界（100.64–100.127 命中；100.63/100.128/非 100 不命中）。
    /// 变异：把范围写成 (64..=126) 或 0..=127 → 边界 case 转红。
    #[test]
    fn is_tailnet_addr_boundaries() {
        assert!(is_tailnet_addr("100.64.0.1"));
        assert!(is_tailnet_addr("100.127.255.255"));
        assert!(!is_tailnet_addr("100.63.0.1"));
        assert!(!is_tailnet_addr("100.128.0.1"));
        assert!(!is_tailnet_addr("192.168.1.1"));
        assert!(!is_tailnet_addr("10.64.0.1"));
    }

    /// 反查优先「起核后新增（不在 baseline）且带 tailnet 地址」的 utun。
    /// 变异：删掉 baseline 过滤（filter）→ 会错命中 baseline 里的 Tailscale.app utun（utun3）→ 转红。
    #[test]
    fn pick_tailnet_iface_prefers_new_utun_over_baseline() {
        // utun3 = 起核前已存在的 Tailscale.app 接口（在 baseline，也带 tailnet 地址）；
        // utun7 = 起核后 sing-box 新建的 TS 接口（不在 baseline）。应挑 utun7。
        let ifaces = vec![
            ("utun3".to_string(), vec!["100.100.0.1".to_string()]),
            ("utun7".to_string(), vec!["100.64.0.9".to_string()]),
        ];
        let mut baseline = HashSet::new();
        baseline.insert("utun3".to_string());
        assert_eq!(
            pick_tailnet_iface(&ifaces, Some(&baseline)).as_deref(),
            Some("utun7"),
            "须优先起核后新增的 utun（时序 diff），不误命中 baseline 里的 Tailscale.app utun"
        );
    }

    /// 无 baseline → 退化为纯地址反推（Polaris 兜底）：取第一张带 tailnet 地址的 utun。
    #[test]
    fn pick_tailnet_iface_falls_back_to_address_when_no_baseline() {
        let ifaces = vec![
            ("utun5".to_string(), vec!["10.0.0.2".to_string()]),
            ("utun6".to_string(), vec!["100.96.1.2".to_string()]),
        ];
        assert_eq!(pick_tailnet_iface(&ifaces, None).as_deref(), Some("utun6"));
        // 无任何 tailnet 地址 → None。
        let none = vec![("utun5".to_string(), vec!["10.0.0.2".to_string()])];
        assert_eq!(pick_tailnet_iface(&none, None), None);
    }
}

#[cfg(test)]
mod ts_status_cache_tests {
    //! A3 缓存面门：末帧缓存读写 + 快照合成（relay ⇄ tailscale_get_status 的中转）。
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-tsstatus-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn event(id: &str, logged_in: bool) -> TailscaleStatusEvent {
        TailscaleStatusEvent {
            server_id: id.to_string(),
            backend_state: if logged_in { "Running" } else { "NeedsLogin" }.to_string(),
            logged_in,
            auth_url: None,
            tailscale_ips: vec!["100.64.0.1".to_string()],
            expired: false,
            peers: Vec::new(),
            // Taildrop 四位在本用例无关，取「无能力、无文件」的中性值；不给 Default 是刻意的：
            // 日后再加字段时，这些构造点必须重新被人看一眼，而不是被 `..Default::default()` 静默补齐。
            can_share_files: false,
            waiting_file_count: 0,
            receiving_file_count: 0,
            unread_file_count: 0,
        }
    }

    /// 空缓存（无帧）→ 快照 statuses 空；connected 透传调用方入参（核 running 态）。
    #[test]
    fn empty_cache_snapshot_is_empty_but_connected_passes_through() {
        let dir = temp_dir("empty");
        let mesh = MeshRuntime::new(dir.clone());
        let snap = mesh.tailscale_status_snapshot(true);
        assert!(snap.connected, "connected 由入参透传（核在跑）");
        assert!(snap.statuses.is_empty(), "无帧 → statuses 空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// update → 快照读回真数据（非恒空）。打断 `update_ts_status` 落库 / `tailscale_status_snapshot` 读缓存 → 转红。
    #[test]
    fn update_then_snapshot_returns_cached_frame() {
        let dir = temp_dir("update");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.update_ts_status(vec![event("srv-a", true), event("srv-b", false)]);
        let snap = mesh.tailscale_status_snapshot(true);
        assert_eq!(snap.statuses.len(), 2, "快照读回缓存末帧（非恒空）");
        assert_eq!(snap.statuses[0].server_id, "srv-a");
        assert!(snap.statuses[0].logged_in);
        assert!(!snap.statuses[1].logged_in);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每帧整体替换（非累加）：第二帧覆盖第一帧。打断「替换」为「追加」→ len 转红。
    #[test]
    fn frame_replaces_wholesale() {
        let dir = temp_dir("replace");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.update_ts_status(vec![event("srv-a", true), event("srv-b", true)]);
        mesh.update_ts_status(vec![event("srv-c", false)]); // 新的全量帧
        let snap = mesh.tailscale_status_snapshot(true);
        assert_eq!(snap.statuses.len(), 1, "全量帧整体替换，非累加");
        assert_eq!(snap.statuses[0].server_id, "srv-c");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 停核 clear → 缓存清空。打断 `clear_ts_status` → 快照仍带陈旧帧 → 转红。
    #[test]
    fn clear_drops_cached_frame() {
        let dir = temp_dir("clear");
        let mesh = MeshRuntime::new(dir.clone());
        mesh.update_ts_status(vec![event("srv-a", true)]);
        mesh.clear_ts_status();
        let snap = mesh.tailscale_status_snapshot(false);
        assert!(!snap.connected);
        assert!(snap.statuses.is_empty(), "清缓存后无陈旧帧");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A4：`selected_exit_backend_state` 读选中出口末帧 backendState。
    #[test]
    fn selected_exit_backend_state_reads_frame() {
        let dir = temp_dir("bstate");
        let mesh = MeshRuntime::new(dir.clone());
        // 无帧 → None。
        assert_eq!(mesh.selected_exit_backend_state("srv-a"), None);
        // 有帧 → 读回 backendState。
        mesh.update_ts_status(vec![event("srv-a", false), event("srv-b", true)]);
        assert_eq!(
            mesh.selected_exit_backend_state("srv-a").as_deref(),
            Some("NeedsLogin")
        );
        assert_eq!(
            mesh.selected_exit_backend_state("srv-b").as_deref(),
            Some("Running")
        );
        // 未在册端点 → None。
        assert_eq!(mesh.selected_exit_backend_state("srv-x"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A4：`expired` 帧即便 backendState=Running 也投影为 `"NeedsLogin"`（key 过期须重登，防死出口黑洞）。
    /// 打断 `selected_exit_backend_state` 的 expired 分支 → 返回 "Running" → 转红。
    #[test]
    fn selected_exit_backend_state_expired_maps_to_needs_login() {
        let dir = temp_dir("expired");
        let mesh = MeshRuntime::new(dir.clone());
        let mut ev = event("srv-a", true); // backend_state=Running, logged_in=true
        ev.expired = true;
        mesh.update_ts_status(vec![ev]);
        assert_eq!(
            mesh.selected_exit_backend_state("srv-a").as_deref(),
            Some("NeedsLogin"),
            "过期 key 须投影为 NeedsLogin，即便帧仍报 Running"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A4 登录期出口让位纯谓词门：`mesh_login_fallback_should_engage` 六条件穷举。
///
/// 变异有牙：从「全命中」基线出发，逐一翻转每个入参 → 结果必翻假（覆盖 6 条逃逸路径，防碰巧真数据对）。
#[cfg(test)]
mod login_fallback_predicate_tests {
    use super::{mesh_login_fallback_should_engage, MeshLoginFallbackInput};

    /// 让位应生效的基线输入：账号制 TS 全隧道出口、开关开、非 direct、无 authKey、未就绪。
    fn engage_baseline() -> MeshLoginFallbackInput {
        MeshLoginFallbackInput {
            fallback_enabled: true,
            proxy_mode_direct: false,
            selected_exit_falls_back_direct: false,
            selected_is_tailscale: true,
            selected_has_auth_key: false,
            selected_tunnel_ready: false,
        }
    }

    #[test]
    fn baseline_engages() {
        assert!(mesh_login_fallback_should_engage(&engage_baseline()));
    }

    /// 逐一翻转 6 个入参 → 结果必翻假。每个 case = 一条独立逃逸路径（删对应 `&&` 项即某 case 转绿→红）。
    #[test]
    fn each_condition_flip_disengages() {
        // (标签, 变异闭包)
        type Mutator = fn(&mut MeshLoginFallbackInput);
        let mutators: [(&str, Mutator); 6] = [
            ("fallback_enabled=false", |i| i.fallback_enabled = false),
            ("proxy_mode_direct=true", |i| i.proxy_mode_direct = true),
            ("falls_back_direct=true", |i| {
                i.selected_exit_falls_back_direct = true
            }),
            ("not_tailscale", |i| i.selected_is_tailscale = false),
            ("has_auth_key=true", |i| i.selected_has_auth_key = true),
            ("tunnel_ready=true", |i| i.selected_tunnel_ready = true),
        ];
        for (label, mutate) in mutators {
            let mut input = engage_baseline();
            mutate(&mut input);
            assert!(
                !mesh_login_fallback_should_engage(&input),
                "翻转「{label}」后必须不让位（该条件是死锁形态的必要项）"
            );
        }
    }
}
