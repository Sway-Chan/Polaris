//! **临时核测速**的宿主层编排（上游 `SpeedTestService.testServersViaProxy`，`SpeedTestService.ts:388-620`）。
//!
//! # 这条腿补的是什么能力
//!
//! 主核**没在跑**时（用户还没点「连接」），`server_speed_test` 此前只能返 clean error「核未运行，无法测速」。
//! 而「先测速比较延迟、再选一个最快的节点连上去」是常规使用序 —— 少了这条腿，用户必须先盲选一个节点连上、
//! 才能测别的节点。本模块起一个**独立的瞬态 sing-box**（每个可测节点一个 HTTP 入站 → 该节点出站），经各自
//! 端口量 warm-TTFB，测完即杀。
//!
//! # 与常驻主核的隔离（三条硬边界，逐条对应一个真实事故面）
//!
//! 1. **独立配置文件**：`<configDir>/speedtest-core.json`，绝不碰主核的 `singbox-runtime.json`
//!    （[`ProxyRuntime::runtime_config_path`](crate::runtime::proxy::ProxyRuntime::runtime_config_path)）。
//! 2. **独立端口**：经 [`PortAllocator::resolve_distinct_free_ports`] 现分配，且**排除**用户配置的
//!    control/http/mixed 口 —— 否则主核随后起来时会撞在临时核占着的口上，表现为「测完速就连不上」。
//! 3. **不写主核的任何生命周期槽**：child 句柄由本模块的会话独占，绝不进 `ProxyRuntime` 的 `pid`/`child`；
//!    也不置 `core_via_helper` 标记。临时核**永不经 helper 起**（无 TUN、无 root 需求）。
//!
//! # 让位语义（§15.11 gen abort 惯例的镜像腿）
//!
//! 主核和临时核**绝不能同时跑**：同一个 WG/WARP peer 被两个会话同时握手会互相踢线（上游 G1 的
//! 「双会话超时」），Tailscale 更是连第二个 tsnet 实例都建不出来。本腿只在主核未跑时开工，且全程守
//! [`is_temp_core_superseded`]：
//!
//! - `gen != gen0` —— 用户中途点了「连接」（`start` 先 bump 世代再动核）⇒ 主核来了，临时核**立刻让路**；
//! - `running == true` —— **世代腿盖不住的那一半**：起核的 bump 可能发生在本次测速取 `gen0` **之前**
//!   （此刻 `status.running` 仍是 false，因为核还在启动中），随后核就绪 ⇒ `running` 翻真而世代不再变。
//!   只查世代的话，这整段窗口里临时核与主核并存 —— 正是双会话事故的形态。
//! - `starting == true` —— **前两条腿同时为假的那整段启动期**：`start` 先置 `start_inflight`
//!   （`starting` 的源）、再跑可达数秒的 stale 清扫、才 `bump_generation`；`gen0` 落在 bump 之后、
//!   就绪之前时，世代腿与 `running` 腿双盲，而主核正在 spawn + bind 端口。
//!
//! 让路 = **中断编排 + 杀临时核 + 未测节点缺席**（不写假 `-1`），与主核池路径 [`drive_pool_waves`] 的
//! 三检查点逐字同义。收尾（杀核 + 删配置）走**无条件**路径，让位/失败/正常完成三条腿共用。
//!
//! [`drive_pool_waves`]: crate::commands::speedtest
//!
//! # 诚实边界（务必读）
//!
//! 本模块的端到端价值 = **真 sing-box + 真出站 + 真网络往返**。这条真机路径**在本 Linux 开发机上无法
//! 验证**（本仓禁跑触碰宿主网络的测试）。因此：
//! - 全部可单测面（节点分区、配置生成、端口/tag 绑定、并发分批、让位三检查点、收尾回收）都以**注入的**
//!   [`TempCoreSpawner`] / 测量闭包 / 事件闭包驱动 —— 无真进程、无网络、无真 sing-box；
//! - **真 spawn + 真延迟数值**一段**在此未验证**，门槛是一次真机会话。不得据本模块宣称「临时核测速端到端可用」。

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

use polaris_config_engine::builder::endpoints::build_wireguard_endpoint;
use polaris_config_engine::builder::outbound::build_proxy_outbound;
use polaris_config_engine::singbox::DomainResolver;
use polaris_config_engine::user_config::server_config::{
    lands_in_endpoints, Protocol, ServerConfig,
};
use polaris_core_supervisor::port_bookkeeping::TokioPortProvider;
use polaris_core_supervisor::{
    wait_for_core_ready, CoreReadyDeps, CoreReadyOutcome, PortAllocator, PortExclusions, Signal,
    SpawnRequest, WaitForCoreReadyOptions,
};

use crate::events::channel::{
    EVENT_SPEED_TEST_DONE, EVENT_SPEED_TEST_PROGRESS, EVENT_SPEED_TEST_RESULT,
};
use crate::runtime::proxy::{pid_alive, send_signal, CoreBuildEnv};
// 瞬态核的进程原语**复用** `tailscale_login_core` 已建好的那一套（spawn → 装箱 child → SIGTERM/宽限/
// SIGKILL/reap）。名字带 "Login" 是历史包袱，语义是「瞬态 sing-box 子进程」，与本腿逐字相同；再写一套
// 进程管理只会多一份要各自维护的收割纪律（而收割写漏的表现是孤儿核，静默且持久）。
use crate::runtime::tailscale_login_core::{
    ConfigChecker, LoginCoreChild, LoginCoreSpawner, SingBoxConfigChecker, TokioLoginCoreSpawner,
};

/// 临时核可测节点的**滑动窗口**上限（对齐 上游 `SpeedTestService.PROXY_TEST_CONCURRENCY = 16`，`:90`）。
///
/// 不设上限时大订阅会把 N 路 TLS/QUIC 握手同时打出去 → 本机 CPU/连接数打满 → 一批**假超时**
/// （节点其实是好的）。≤上限的小订阅等价于全并行，零代价。
///
/// 语义是「**同时在飞**至多这么多」（回来一个补一个），**不是**「切成这么大的批」——
/// 见 [`drive_temp_core_measures`] 的调度形态一节。
pub const TEMP_CORE_CONCURRENCY: usize = 16;

/// 临时核就绪等待上限（对齐 上游 `waitForPortReady(ports[0], 10000)`，`:510`）。
/// 应用分流规则集/geo 资源的加载可能耗时，给 10s。
const TEMP_CORE_READY_TIMEOUT_MS: u64 = 10_000;

/// 就绪轮询间隔。
const TEMP_CORE_READY_POLL_MS: u64 = 200;

/// **在飞**让位轮询间隔（[`drive_temp_core_measures`] 的检查点②）。
///
/// 只在「发新活之前 / 每节点测完」两处查是不够的：窗口里的节点**全部不可达**时（真机上就是订阅里
/// 有 ≥16 个死节点），那两处一个都醒不过来 —— supersede 信号出现后临时核（及其**已建立的 WG/WARP
/// 会话**）还要活满一整个测量超时。Linux/macOS 靠主核 `start()` 入口的 stale sweep 顺带杀掉——那是
/// **副作用缓解、不是设计保证**；Windows 无 sweep（`scan_running_cores` 恒返空）⇒ 全程重叠。
/// 故按本间隔独立轮询（`timeout(poll, join_next())`，**不依赖任何测量返回**），命中即 `abort_all` +
/// 立即返回（调用方紧接着 `terminate()`）。
const TEMP_CORE_SUPERSEDE_POLL_MS: u64 = 200;

/// 临时核配置文件名（**独立于**主核 `singbox-runtime.json`）。
///
/// 固定名而非带时间戳（上游 `speedtest_${Date.now()}.json`）：测速已有进程级单飞闸
/// （`commands::speedtest::SpeedTestGuard`）⇒ 同时至多一个临时核，固定名不会自撞，且上次会话崩溃残留的
/// 那份会被本次直接覆盖（带时间戳反而会在 config 目录里越堆越多）。
const TEMP_CORE_CONFIG_NAME: &str = "speedtest-core.json";

/// **在飞临时核 pid 表** —— 应用退出清理的唯一真值源。
///
/// # 为什么光有 child 的 `Drop` 守卫不够
///
/// 临时核 child 由本模块的会话 future 独占持有，`TokioLoginCoreChild` 的 Drop 守卫只覆盖「future 被
/// 丢弃 / panic 展开」。**应用退出**走的是 `RunEvent::ExitRequested → run_exit_cleanup → 进程退出`，
/// 在飞的 tokio task **根本不会被 drop** ⇒ 临时核不随父进程死，留下一个持续持有 N 个回环端口 +
/// WG/WARP peer 会话的孤儿 sing-box。而兜底 sweep 只在**下次** `start()` 才跑，且 Windows 的
/// `scan_running_cores` 恒返空（`core-supervisor/src/stale_core.rs`：`tasklist` 不输出命令行，无从
/// 施加「只杀本 app 起的核」判据）⇒ **Windows 孤儿永不被清**。
///
/// 故在此登记 pid，由 `main.rs::run_exit_cleanup` 经 [`kill_inflight_temp_cores`] 收口。
static INFLIGHT_TEMP_CORES: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// 取 pid 表锁（临界区极短、绝不跨 await；中毒仍恢复内层，不为一条清理路径 panic 掉退出流程）。
fn temp_core_pids() -> MutexGuard<'static, BTreeSet<u32>> {
    INFLIGHT_TEMP_CORES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 排空 pid 表（**不发任何信号**）。退出清理与单测共用同一个真值源 —— 单测只走本函数即可观测
/// 注册/注销，绝不对真实进程发信号（本仓禁在单测里碰宿主进程/网络）。
fn take_inflight_temp_core_pids() -> Vec<u32> {
    std::mem::take(&mut *temp_core_pids()).into_iter().collect()
}

/// **应用退出清理**：SIGKILL 掉全部在飞临时核，返回实际发信号条数（0 = 退出时没有测速在飞）。
///
/// 直接 SIGKILL 不走 SIGTERM 宽限：退出路径不能再等一个 5s 宽限窗；临时核无状态（配置随后即删、
/// 不写主核任何生命周期槽），强杀无副作用。
pub fn kill_inflight_temp_cores() -> usize {
    kill_temp_cores_with(|pid| send_signal(pid, Signal::Sigkill))
}

/// [`kill_inflight_temp_cores`] 的可注入内核（**收割动作是唯一注入点**）：单测传记录闭包驱动整条
/// 「排空 → 逐 pid 收割 → 计数」逻辑，**不对任何真实进程发信号**（本仓禁在单测里碰宿主进程）。
fn kill_temp_cores_with(mut kill: impl FnMut(u32)) -> usize {
    let pids = take_inflight_temp_core_pids();
    for pid in &pids {
        log::warn!("退出清理：强杀在飞测速临时核 pid={pid}");
        kill(*pid);
    }
    pids.len()
}

/// pid 登记 RAII 守卫：`drive_after_spawn` 的每一条 return / panic 展开 / future 被丢弃都会注销，
/// 故表里只会留下**此刻真在飞**的 pid（退出清理据此发信号，pid 复用误杀窗口被压到最小）。
struct TempCorePidGuard(u32);

impl TempCorePidGuard {
    /// 登记一个 pid；`pid == 0`（取不到 pid / 测试假核）→ 不登记（返 `None`）。
    fn register(pid: u32) -> Option<Self> {
        (pid != 0).then(|| {
            temp_core_pids().insert(pid);
            Self(pid)
        })
    }
}

impl Drop for TempCorePidGuard {
    fn drop(&mut self) {
        temp_core_pids().remove(&self.0);
    }
}

/// 临时核**入站→出站** 1:1 绑定的一个节点（[`plan_temp_core`] 产出，[`build_temp_core_config`] 消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempNode {
    /// 节点 id（结果回填键 + `event:speedTestResult` 的 serverId）。
    pub id: String,
    /// 临时核内的出站/端点 tag（`out-<id 前 8 位>`，见 [`temp_core_tag`]）。
    pub tag: String,
    /// 预构造的出站（普通协议）或端点（WG / 自定义 endpoint）JSON。
    pub node: Value,
    /// 是否走 `endpoints[]`（L3 端点，须额外配穿隧道 DNS，见 [`build_temp_core_config`]）。
    pub is_endpoint: bool,
    /// WG 本地地址含 IPv6（端点 DNS 族别偏好的分流，对齐 上游 `:868`）。
    ///
    /// 纯 v4 ⇒ 给该入站前置一条 AAAA `predefined` 空答复（等价旧 `ipv4_only`）；
    /// 含 v6 ⇒ 不下发任何东西（等价旧 `prefer_ipv4`，见 [`build_temp_core_config`]）。
    pub has_local_v6: bool,
}

/// 临时核**不可测**节点的原因（如实回报，绝不伪造 `-1`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempSkip {
    /// 协议是 `tailscale`：临时核建不出第二个 tsnet 实例，且会与主核抢同一份 `tailscale-state` 目录。
    /// 对齐 上游 `:242-247` 的漂移防护（`isSpeedTestable(s, { mainCorePool: false })` 剔 TS-exit）。
    Tailscale,
    /// 协议是 `naive` 但本机没有 libcronet：该节点进临时核会让核**预初始化 FATAL**，拖垮整批
    /// （对齐 上游 `:438` 的「不可用节点不进临时核」）。
    NaiveWithoutCronet,
    /// 出站/端点构造失败（WG 缺 privateKey / 自定义 JSON 形态非法等）。
    BuildFailed,
}

/// [`plan_temp_core`] 的产出：可测节点（保序）+ 各原因的缺席列表（保序）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TempCorePlan {
    /// 进临时核真测的节点。
    pub testable: Vec<TempNode>,
    /// 因协议是 tailscale 而缺席（回报进响应的 `tsNotReady`，对齐 上游 L-2 `:248-250`）。
    pub tailscale: Vec<String>,
    /// 因 naive 缺 cronet / 构造失败而缺席（回报进响应的 `notInPool`：对用户同样是「本轮没测」）。
    pub unusable: Vec<String>,
}

/// 临时核里某节点的 **基础** tag（对齐 上游 `out-${s.id.slice(0, 8)}`，`:443`）。
///
/// 取 id 前 8 位而非全 id：sing-box tag 只需在**本临时核内**唯一，而 id 是 uuid ⇒ 前 8 位碰撞概率可忽略，
/// 短 tag 让核日志与 DNS 规则可读。**碰撞真发生时**由 [`unique_temp_core_tag`] 加序号消歧，绝不生成两个
/// 同 tag 的出站（那会让核启动直接 FATAL、整批测不成）。
#[must_use]
pub fn temp_core_tag(id: &str) -> String {
    let head: String = id.chars().take(8).collect();
    format!("out-{head}")
}

/// 在 `taken` 之外取一个唯一 tag：基础 tag 空着就用它，否则加序号（`out-xxxxxxxx-2`、`-3`…）。
///
/// # 为什么不是「后来者出局」
///
/// id **不保证是 uuid**：手输/导入的节点常见 `mynode-a1` / `mynode-a2` 这种前缀相同的命名，前 8 位
/// 逐字相同 ⇒ 碰撞不是「概率可忽略」而是**确定性**发生。旧的去重腿把后来者整个丢进 `unusable`，
/// 用户侧表现是那个节点**每次**都以笼统的 `notInPool` 缺席、且无从修复（他不知道要去改 id 前 8 位）。
/// 消歧后两个节点各有独立入站/出站/DNS 规则，照常各测各的。
///
/// `taken` 有限 ⇒ 循环必然终止。
fn unique_temp_core_tag(id: &str, taken: &BTreeSet<String>) -> String {
    let base = temp_core_tag(id);
    if !taken.contains(&base) {
        return base;
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// 把请求集裁成「进临时核的节点」+「缺席列表」（纯逻辑，对齐 上游 `:438-450` 的 `usable` 循环）。
///
/// 判定序（每一条都对应一个真实的整批失败面，顺序不可换）：
/// 1. **tailscale → 缺席**：临时核建不出第二个 tsnet 实例；即便建得出，它与主核共用 `tailscale-state`
///    目录，两个核同写必然把登录态写坏。这条**必须在构造之前**判 —— 构造它本身就要落状态目录。
/// 2. **naive 且无 cronet → 缺席**：进核会 FATAL 拖垮**整批**（不是只坏它自己）。
/// 3. **tag 消歧**：同 tag 两个出站 ⇒ 核启动 FATAL。id 前 8 位碰撞时加序号
///    （[`unique_temp_core_tag`]），**不丢节点**。
/// 4. **构造失败 → 缺席**：WG 缺 privateKey / 自定义 JSON 形态非法。绝不放一个半截出站进核。
#[must_use]
pub fn plan_temp_core(servers: &[ServerConfig], env: &CoreBuildEnv) -> TempCorePlan {
    let mut out = TempCorePlan::default();
    let mut seen_tags: BTreeSet<String> = BTreeSet::new();
    for s in servers {
        if s.protocol == Protocol::Tailscale {
            out.tailscale.push(s.id.clone());
            continue;
        }
        if s.protocol == Protocol::Naive && !env.has_cronet {
            out.unusable.push(s.id.clone());
            continue;
        }
        let tag = unique_temp_core_tag(&s.id, &seen_tags);
        seen_tags.insert(tag.clone());
        match build_temp_node(s, &tag, env) {
            Some(node) => out.testable.push(node),
            None => {
                // tag 已占坑但节点没建成 → 归还，免得后一个真能建成的同 tag 节点被误判成碰撞。
                seen_tags.remove(&tag);
                out.unusable.push(s.id.clone());
            }
        }
    }
    out
}

/// 单节点的出站/端点构造（复用 config-engine 的 20 协议字段映射，**不在本层重写任何协议细节**）。
///
/// `domain_resolver` 一律指向临时核自己的 `dns-direct`（223.5.5.5）—— 那是**节点 server 地址**的解析器，
/// 与「目标域名怎么解析」是两回事（见 [`build_temp_core_config`] 的两类解析不变量）。
fn build_temp_node(s: &ServerConfig, tag: &str, env: &CoreBuildEnv) -> Option<TempNode> {
    let is_custom_endpoint = s.protocol == Protocol::Custom
        && s.custom_settings
            .as_ref()
            .and_then(|c| c.is_endpoint)
            .unwrap_or(false);

    if s.protocol == Protocol::Wireguard {
        // detour 恒传 `None`：临时测速核只装被测节点自己 + `dns-direct`，前置代理那个 outbound
        // 压根不在这份配置里 —— 填了就是指向不存在的 tag ⇒ FATAL。判据同下面自定义 endpoint 腿的
        // `obj.remove("detour")`（那条注释写的就是这件事），两条腿口径一致。
        // 代价：带前置代理的 WG 节点，测得的是**直连**该 peer 的速度，不是经链路的速度。
        // dial 侧解析器传**纯 tag**，不是 #335 的结构化 `{server, strategy}` 形态：那条缺陷的根因是
        // 「顶层 `dns.strategy=ipv4_only` 连带压掉节点域名的 AAAA」，而临时测速核**恒不下发顶层
        // `dns.strategy`**（见下方 `build_temp_core_config` 的不变量注释，变异锁单测
        // `temp_core_dns_never_sets_a_legacy_or_top_level_strategy` 断言 `dns.strategy` 必须缺席）
        // ⇒ 这份配置里没有可覆盖的顶层策略，下发结构化形态属无据的行为变更。
        let ep = build_wireguard_endpoint(
            s,
            tag,
            Some(&DomainResolver::Tag(DIRECT_DNS_TAG.to_string())),
            &env.platform,
            None,
        )
        .ok()?;
        let has_local_v6 = s
            .wireguard_settings
            .as_ref()
            .is_some_and(|w| w.local_address.iter().any(|a| a.contains(':')));
        return Some(TempNode {
            id: s.id.clone(),
            tag: tag.to_string(),
            node: serde_json::to_value(ep).ok()?,
            is_endpoint: true,
            has_local_v6,
        });
    }

    if is_custom_endpoint {
        // 自定义 endpoint：原样透传用户 JSON，仅覆盖 tag、剥内层 detour（对齐 config-engine
        // `build_outbounds` 的自定义 endpoint 腿；detour 在临时核里指向不存在的 tag 会 FATAL）。
        let mut val = s.custom_settings.as_ref()?.outbound.clone();
        let obj = val.as_object_mut()?;
        obj.remove("detour");
        obj.insert("tag".into(), Value::from(tag));
        return Some(TempNode {
            id: s.id.clone(),
            tag: tag.to_string(),
            node: val,
            is_endpoint: true,
            has_local_v6: false,
        });
    }

    // 纯 tag 而非 #335 的结构化形态，理由同上面 WG 那条腿（临时核无顶层 `dns.strategy` 可覆盖）。
    let ob = build_proxy_outbound(
        s,
        tag,
        &DomainResolver::Tag(DIRECT_DNS_TAG.to_string()),
        &env.arch,
        &env.platform,
    );
    // **detour 一律剥掉**：临时核只装被测节点自己，链式前置节点的 tag 在核里根本不存在 ⇒ 留着必 FATAL。
    // 代价是「代理链节点测的是它自己那一跳」——如实、且与旧行为（根本测不了）相比只增不减。
    let mut val = serde_json::to_value(ob).ok()?;
    if let Some(obj) = val.as_object_mut() {
        obj.remove("detour");
    }
    // 判据是 `lands_in_endpoints`（JSON 该塞哪个数组），不是组网资格。从前用的是只认 WG/TS 的那个
    // 谓词 ⇒ openconnect / openvpn-client 被塞进临时核的 `outbounds[]`，内核 decode 阶段判
    // `unknown outbound type` —— **整个临时核起不来**，同批被测的其它节点一并测不成。
    let is_endpoint = lands_in_endpoints(s.protocol);
    Some(TempNode {
        id: s.id.clone(),
        tag: tag.to_string(),
        node: val,
        is_endpoint,
        has_local_v6: false,
    })
}

/// 临时核里解析**节点 server 地址**用的 DNS server tag（223.5.5.5，本机直发）。
const DIRECT_DNS_TAG: &str = "dns-direct";

/// 生成临时核 sing-box 配置（纯逻辑，1:1 上游 `generateProxyTestConfig`，`:808-890`）。
///
/// 形状：每个可测节点一个 `http` 入站（`127.0.0.1:<port>`）→ `route.rules` 按 `inbound` 指到该节点 tag。
/// 端点类节点另进 `endpoints[]`，普通协议进 `outbounds[]`。
///
/// # 两类解析不变量（上游 issue #154 + 2026-07 端点修正，真机 debug 确证 —— 勿动）
///
/// - **代理出站**（vless/vmess/trojan/hy2/tuic/ss/…）：目标域名以 `ATYP=domain` **透传给出口远程解析**，
///   不经本机 `dns-direct`。各节点因此量到自身真实路径。⚠️ 勿引入 `sniff` / `outbound.domain_strategy` /
///   任何针对**目标**的本地解析 —— 会破坏此不变量，把所有节点测成同一条本机解析路径。
/// - **端点**（WG/WARP… L3）：内核**强制本地解析**目标域名。默认 `dns-direct` 从**本机**解析 ⇒ 拿到的是
///   本机地理的 IP，而端点出口可能在别处（境外 WARP / 国内自建 WG）⇒ 够不着 → 超时/失真。故按 `inbound`
///   键控一条 DNS 规则，把该端点的目标解析定向到**穿本隧道**的 223.5.5.5（AliDNS 有大陆 PoP + ECS，
///   按**出口地理**返 IP，境内外单形态覆盖）。`disable_cache` 必开：多端点并测时各自的答案不同，共享缓存
///   会互相污染。
///
/// `ports[i]` 与 `nodes[i]` **逐位 1:1**（调用方保证等长；短了则多出的节点不生成入站 —— 由
/// [`TempCoreSession::run`] 的等长断言挡在生成之前）。
#[must_use]
pub fn build_temp_core_config(nodes: &[TempNode], ports: &[u16], log_level: &str) -> Value {
    let mut inbounds = Vec::new();
    let mut outbounds = Vec::new();
    let mut endpoints = Vec::new();
    let mut route_rules = Vec::new();
    let mut dns_servers = vec![json!({
        "tag": DIRECT_DNS_TAG, "type": "udp", "server": "223.5.5.5", "server_port": 53,
    })];
    let mut dns_rules: Vec<Value> = Vec::new();

    for (node, port) in nodes.iter().zip(ports.iter()) {
        let inbound_tag = format!("in-{}", node.tag);
        inbounds.push(json!({
            "type": "http", "tag": inbound_tag, "listen": "127.0.0.1", "listen_port": port,
        }));
        route_rules.push(json!({
            "inbound": [inbound_tag], "action": "route", "outbound": node.tag,
        }));
        if node.is_endpoint {
            let exit_dns_tag = format!("dns-exit-{}", node.tag);
            dns_servers.push(json!({
                "tag": exit_dns_tag, "type": "udp", "server": "223.5.5.5", "server_port": 53,
                // 查询穿本端点隧道 → AliDNS 按出口地理（ECS）返 IP。
                // ⚠️ 端点级 `domain_resolver` 只管 peer 地址，**禁**指向隧道 DNS（peer 解析死锁 FATAL，实测）。
                "detour": node.tag,
            }));
            // 族别偏好（语义不变，写法迁移；1:1 上游 `0875f66`(#334)，`SpeedTestService.ts:850-881`）。
            //
            // **为什么不能留 legacy rule-action `strategy`**：sing-box 1.14.0 起 ① `run` 输出 deprecation
            // 警告、**1.16.0 移除**（`check` 静默放行 ⇒ 我们起核前那道 `sing-box check` 抓不到）；
            // ② 它与**同一份 dns 配置内**任何带 `query_type`/`ip_version` 的规则**互斥**，共存即
            // `initialize dns router` FATAL、`check` 与 `run` 双双硬拒。本配置今天零 `query_type`，
            // 故只是**恰好**没踩上——往临时核 DNS 规则里加任何一个 query_type 字段即整批测速起核 FATAL。
            //
            //  · 旧 `prefer_ipv4`（localAddress 含 v6）→ **不下发任何东西**：本配置无顶层 `dns.strategy`
            //    （见下方 `dns` 组装），内核默认并发 A/AAAA 且把 v4 排在 v6 前（`sortAddresses` 对
            //    AsIS 与 prefer_ipv4 同一分支）。⚠️ 该等价性**依赖测速配置不带顶层 dns.strategy**，
            //    由 `temp_core_dns_never_sets_a_top_level_strategy` 锁死。
            //  · 旧 `ipv4_only`（纯 v4）→ 给该 inbound 的 AAAA 查询前置一条 `predefined` 空 NOERROR：
            //    AAAA 就地返空、不出网，结果集只剩 A。
            //
            // 顺序有牙：抑制规则必须排在本节点 route 规则**之前** —— DNS 规则先匹配先命中，route 规则是
            // 该 inbound 的 catch-all，排它后面则 AAAA 先被 route 吃掉、抑制静默失效（且配置照样过校验）。
            if !node.has_local_v6 {
                dns_rules.push(json!({
                    "inbound": [inbound_tag], "query_type": ["AAAA"],
                    // 空答复：等价旧 ipv4_only 的「不要 v6」，且不触发拒绝日志噪声。
                    "action": "predefined", "rcode": "NOERROR",
                }));
            }
            dns_rules.push(json!({
                "inbound": [inbound_tag], "action": "route", "server": exit_dns_tag,
                "disable_cache": true,
            }));
            endpoints.push(node.node.clone());
        } else {
            outbounds.push(node.node.clone());
        }
    }

    // sing-box 启动要求至少一个 direct 出站（也是 DNS 直发腿的落点）。
    outbounds.push(json!({ "type": "direct", "tag": "direct" }));

    // ⚠️ **恒不下发顶层 `dns.strategy`**：端点族别偏好靠上面的 `query_type` 规则项表达，而「无顶层
    // strategy」正是「省略 == 旧 prefer_ipv4」这条等价性的前提（顶层若为 prefer_ipv6，端点解析会翻成
    // v6 优先）。要加顶层 strategy 必须同时重新推导端点规则，别只加一半。单测锁死本不变量。
    let mut dns = json!({ "servers": dns_servers });
    if !dns_rules.is_empty() {
        dns["rules"] = Value::Array(dns_rules);
    }
    let mut cfg = json!({
        "log": { "level": log_level, "timestamp": true },
        "dns": dns,
        "inbounds": inbounds,
        "outbounds": outbounds,
        "route": {
            "rules": route_rules,
            "auto_detect_interface": true,
            "default_domain_resolver": DIRECT_DNS_TAG,
        },
    });
    if !endpoints.is_empty() {
        cfg["endpoints"] = Value::Array(endpoints);
    }
    cfg
}

/// **临时核让位判据**（纯逻辑；[`crate::commands::speedtest`] 的 `is_superseded` 的镜像腿）。
///
/// 三条腿的**析取**，缺一不可 —— 且后两条与主核路径**方向相反**，这不是笔误：
/// - `gen_now != gen0`：主核 start/stop/restart/regen 跃迁 ⇒ 主核来了，临时核必须让路；
/// - `running`：主核**已经跑起来了**。世代腿盖不住的窗口是「bump 发生在本次取 `gen0` **之前**」——
///   那一刻 `status.running` 还是 false（核在启动中），我们照常起了临时核；随后核就绪，`running` 翻真
///   而世代不再变化。只查世代 ⇒ 两个核并存跑同一批 WG/WARP peer（上游 G1 双会话事故的形态）。
/// - `starting`：**前两条腿都盖不住的那整段启动期**。`ProxyRuntime::start` 的顺序是
///   `start_inflight+1`（`starting` 的源）→ **stale 清扫（可达数秒）** → `bump_generation` → spawn →
///   就绪门。若本次测速的 `gen0` 恰好取在「bump 之后、核就绪之前」，世代腿与 `running` 腿**同时**为假，
///   而主核正在起：用户点「连接」后紧接点测速（或托盘/另一窗口点，UI 灰态拦不住跨窗）就是确定性命中。
///   后果有两层：① 临时核与启动中的主核并存 ⇒ 同 peer 双会话踢线；② 临时核端口只排除
///   control/http/mixed，会抢走主核刚解析、尚未 bind 的 api/update-in/probe 池口 ⇒ 主核起核
///   FATAL address-in-use（用户看到的是「连接失败」，归因极难）。
///
/// 主核池路径的第二条腿是 `!running`（守的是「核崩了」），本腿是 `running`/`starting`（守的是「核来了」）
/// —— 因为两条腿的**前提**相反：那边跑在核活着的前提上，这边跑在核不在的前提上。
#[must_use]
pub const fn is_temp_core_superseded(
    gen_now: u64,
    gen0: u64,
    running: bool,
    starting: bool,
) -> bool {
    gen_now != gen0 || running || starting
}

/// **临时核测量编排核**（测量 / 事件发射 / 让位三个 I/O 面**全部注入** ⇒ 无 `AppHandle`、无进程、
/// 不碰宿主网络、可单测）。
///
/// # 调度形态：**滑动窗口**（≤`concurrency` 在飞，回来一个补一个）
///
/// = 上游 `runWithLimit`（`SpeedTestService.ts:1331-1344`，调用点 `:530`）的固定 worker 池。
/// 此前是**批屏障**（切成 16 个一批、整批 join 完才发下一批），两者的 makespan 差 W = ⌈N/K⌉ 倍：
///
/// - worker 池的下界是 `max(单点最坏, 总功/并发)` —— 一个测不通的死节点只占住 1/K 的算力；
/// - 批屏障是 `Σ 每批最大值` —— 一个死节点把**整批 K 个**的耗时钉死在超时上限。
///
/// 而「每批至少一个死节点」的概率 = `1-(1-f)^K`，f=0.2、K=16 时是 **0.97** —— 即中等失效率的订阅
/// 几乎每一批都被超时值封顶。N=50/K=16/f=0.2 的模型：批屏障 4 批 × 8s = 32s，滑动窗口
/// `max(8, 40×0.5+10×8 /16) = 8s`。
///
/// # 让位（**这段是本函数的事故面，改前先读完**）
///
/// 主核和临时核**绝不能同时跑**：同一个 WG/WARP peer 被两个会话同时握手会互相踢线，且临时核端口
/// 只排除 control/http/mixed，会抢走主核尚未 bind 的口 ⇒ 主核起核 FATAL。所以「主核来了就立刻停」
/// 不是优化，是**正确性**。三个检查点覆盖全程，缺一即静默重叠：
///
/// 1. **发新活之前**（每轮补位一次）：主核已起 → 停发新活 + `abort_all` 已在飞的，未测节点缺席。
///    这条替代了旧的「批首」检查，粒度**更细**：旧的是每 K 个节点一次，现在是每次补位一次。
/// 2. **在飞轮询**（每 [`TEMP_CORE_SUPERSEDE_POLL_MS`] 一次）：命中即 `abort_all` + 立刻返回。
///    **这条是唯一不依赖任何测量返回的腿** —— 窗口里 16 个全挂死（真机上就是 16 个不可达节点）时，
///    上面两条都醒不过来，只有它按间隔醒。批屏障时代它挂在「批内」，现在挂在整轮，覆盖面只增不减：
///    以前批与批之间那一小段没有轮询（靠批首查兜），现在全程都在轮询窗口里。
///    实现仍用 `timeout(poll, join_next())` 而非 `select!`：`join_next()` 已借走 `set`，`select!` 的
///    另一臂里再调 `set.abort_all()` 会撞借用检查；`join_next` 是 cancel-safe（tokio 文档明载），
///    超时丢弃不丢结果。
/// 3. **每节点测完**：该节点的测量在飞期间主核起来 ⇒ 这个值量的是**与主核抢同一条 peer 会话**的
///    临时核出站，丢弃（并中止其余在飞）。
///
/// 收尾（杀核）由调用方 [`TempCoreSession`] 的**无条件**路径负责，不在本函数。
///
/// **未测节点一律缺席，绝不写假 `-1`** —— 「让位未测」与「真实超时」不可混淆，同主核路径的诚实性根基。
/// 返回 `(结果 map, outcome)`；任一检查点命中即 `interrupted`。
///
/// # 回填粒度：**逐节点**（对齐 上游 `SpeedTestService.ts:564`）
///
/// 每个节点测完那一刻就落账 + 推事件。统一回填的话首个延迟数字要等最慢的那个，屏幕先空十几秒。
/// 代价：让位③是「逐节点级」—— 已回填的不可撤回，丢弃的只是尚未回来的在飞值。这正是 上游的语义
/// （`:541-545` 的超代再检也在 worker 体内、`report()` 之前）。
///
/// # 为什么主核池路径**不**跟着改
///
/// 那边的槽 ↔ 端口是 1:1 硬绑定，跨波复用同槽必须先测完再重指，波屏障是**正确性要求**而非性能选择
/// （上游 同样是波屏障，`SpeedTestService.ts:709-776`）。本腿没有这个约束：每个节点有**自己**的
/// 入站端口，全程不复用。
/// # 终态事件的唯一出口就在本函数
///
/// 内核 [`drive_temp_core_measures_inner`] 有 4 个 `return`（让位三检查点 + 正常收尾），本薄壳把它们
/// 收成一个出口再发 [`EVENT_SPEED_TEST_DONE`] ⇒ 「中断了却没发终态」在结构上写不出来。
/// 载荷含未测集合（续测输入），判据见 [`emit_speed_test_done`]。
pub async fn drive_temp_core_measures<Meas, MeasFut>(
    nodes: &[TempNode],
    ports: &[u16],
    concurrency: usize,
    superseded: &(dyn Fn() -> bool + Sync),
    measure: Meas,
    emit: &mut (dyn FnMut(&str, Value) + Send),
) -> (serde_json::Map<String, Value>, &'static str)
where
    Meas: Fn(u16) -> MeasFut,
    MeasFut: Future<Output = Option<u32>> + Send + 'static,
{
    // 本腿「已裁定要测」的集合 = 前 `total` 个节点（`nodes`/`ports` 逐位 1:1，多出的一侧不测）。
    let intended: Vec<String> = nodes
        .iter()
        .take(nodes.len().min(ports.len()))
        .map(|n| n.id.clone())
        .collect();
    let (results, outcome) =
        drive_temp_core_measures_inner(nodes, ports, concurrency, superseded, measure, emit).await;
    emit_speed_test_done(emit, outcome, &results, &intended);
    (results, outcome)
}

async fn drive_temp_core_measures_inner<Meas, MeasFut>(
    nodes: &[TempNode],
    ports: &[u16],
    concurrency: usize,
    superseded: &(dyn Fn() -> bool + Sync),
    measure: Meas,
    emit: &mut (dyn FnMut(&str, Value) + Send),
) -> (serde_json::Map<String, Value>, &'static str)
where
    Meas: Fn(u16) -> MeasFut,
    MeasFut: Future<Output = Option<u32>> + Send + 'static,
{
    let mut results = serde_json::Map::new();
    let total = nodes.len().min(ports.len());
    let mut tested = 0usize;
    let mut ok = 0usize;

    // `concurrency == 0` → 视作 1：绝不退化成「一个都不测」（那会零事件 ⇒ 前端测速按钮永久卡灰）。
    // 0 并发是配置错误，不是「不测」的意思。
    let window = concurrency.max(1);
    let mut set = tokio::task::JoinSet::new();
    let mut next = 0usize; // 下一个待发的节点下标（ports/nodes 逐位 1:1，全程不复用）

    while !set.is_empty() || next < total {
        if next < total {
            // ── 让位①（发新活之前）：主核已起/已跃迁 → 停发新活 + 中止在飞，未测节点缺席 ──
            if superseded() {
                set.abort_all();
                return (results, "interrupted");
            }
            // 补位：起手补满窗口，此后回来一个补一个。
            while next < total && set.len() < window {
                let node_id = nodes[next].id.clone();
                let fut = measure(ports[next]);
                set.spawn(async move { (node_id, fut.await) });
                next += 1;
            }
        }

        let poll = Duration::from_millis(TEMP_CORE_SUPERSEDE_POLL_MS);
        match tokio::time::timeout(poll, set.join_next()).await {
            // 窗口已空且无待发（上面刚补过位）⇒ 全部收尾。
            Ok(None) => break,
            Ok(Some(Ok((id, latency)))) => {
                // ── 让位③（每节点测完即查）──
                if superseded() {
                    set.abort_all();
                    return (results, "interrupted");
                }
                record_measured(
                    &mut results,
                    &mut tested,
                    &mut ok,
                    emit,
                    &id,
                    latency,
                    total,
                );
            }
            // JoinError（panic / 本函数自己 abort 掉的）→ 该节点无数值，缺席，绝不补 -1。
            Ok(Some(Err(_))) => {}
            // ── 让位②（在飞轮询）：**不依赖任何测量返回**，窗口全挂死时也照样醒 ──
            Err(_elapsed) => {
                if superseded() {
                    set.abort_all();
                    return (results, "interrupted");
                }
            }
        }
    }

    (results, "completed")
}

/// 单个节点的落账 + 推事件（`result` 与 `progress` 成对，计数在此处自增 ⇒ 恒单调）。
///
/// 与主核池路径 [`crate::commands::speedtest`] 的同名函数逐字同义 —— 两条腿的事件形状必须一致，
/// 前端 `use-latency-store` / `NodesScreen` 只有一套消费逻辑。
///
/// `latency == None` ⇒ 记 -1（**真实**不可测：超时 / 传输错）。「让位未测」的节点根本不会走到这里。
fn record_measured(
    results: &mut serde_json::Map<String, Value>,
    tested: &mut usize,
    ok: &mut usize,
    emit: &mut (dyn FnMut(&str, Value) + Send),
    node_id: &str,
    latency: Option<u32>,
    total: usize,
) {
    let latency_val = latency.map_or(-1_i64, i64::from);
    if latency.is_none() {
        log::debug!(
            "临时核测速未取得有效延迟：nodeId={node_id}（可能为冷建链/复用请求超时、传输错误或测速端点非 2xx）"
        );
    }
    results.insert(node_id.to_string(), json!(latency_val));
    emit(
        EVENT_SPEED_TEST_RESULT,
        json!({ "serverId": node_id, "latency": latency_val }),
    );
    *tested += 1;
    if latency.is_some() {
        *ok += 1;
    }
    emit(
        EVENT_SPEED_TEST_PROGRESS,
        json!({ "tested": *tested, "ok": *ok, "total": total }),
    );
}

/// 一轮测速的结果口径三分（供 [`log_speed_test_summary`]；纯函数，可单测）。
///
/// **`-1` 与「缺席」是两件不同的事，混起来就没法排查**：
/// - `ok`：真测出了值（毫秒 ≥ 0）；
/// - `failed`：真测了但没通（`-1` —— 超时 / 传输错 / 非 2xx，见 `measure_via_local_proxy`）；
/// - `absent`：**根本没测**（波前让位 / 中断 / 起测即知不可测）——绝不写假 `-1`，故不在 `results` 里。
///
/// 非数值 / 越界的值一律计入 `failed`（宁可报多也不静默丢：这一层不该有非数值，出现即是缺陷信号）。
#[derive(Debug, PartialEq, Eq)]
pub struct SpeedTestSummary {
    pub ok: usize,
    pub failed: usize,
    pub absent: usize,
}

#[must_use]
pub fn summarize_speed_test(
    results: &serde_json::Map<String, Value>,
    intended: &[String],
    absent: usize,
) -> SpeedTestSummary {
    let ok = results
        .values()
        .filter(|v| v.as_i64().is_some_and(|ms| ms >= 0))
        .count();
    SpeedTestSummary {
        ok,
        failed: results.len() - ok,
        absent,
    }
    .also_assert_total(intended.len())
}

impl SpeedTestSummary {
    /// 三类之和必须等于请求数 —— 不等即口径漏了一类（debug 构型下当场炸，release 只记警告）。
    fn also_assert_total(self, total: usize) -> Self {
        let sum = self.ok + self.failed + self.absent;
        debug_assert_eq!(sum, total, "测速结果三分之和必须等于请求数");
        if sum != total {
            log::warn!("测速结果口径不自洽：ok+failed+absent={sum} ≠ 请求 {total}（{self:?}）");
        }
        self
    }
}

/// 一轮测速的**结果级**日志（唯一出口，三条腿共用 —— 挂在 [`emit_speed_test_done`] 里）。
///
/// # 这条补的是什么洞
///
/// 本链此前**零结果级日志**：机器上只有「测速临时核已 spawn：126 个节点」和「已回收：
/// outcome=completed」两行，中间什么都没有。陈先生 2026-08-02 报「全部测速全部显示 -1，跟实际不符」
/// 时，磁盘上拿不出任何东西能分辨三种完全不同的成因 ——
/// ① 网络真的全失败；② 本轮被让位/中断（节点根本没测，前端把**缺席**画成了 `-1`）；
/// ③ 少数失败但 UI 全渲染成 `-1`。`latency` 又不落 `config.json`（纯渲染端 map），
/// 事后无从复盘。汇总一行即可把三者分开。
///
/// 失败样本只带前 5 个 id：全量在 126 节点时是一行几 KB 的日志，而排查只需要「是不是集中在某一类」。
fn log_speed_test_summary(
    outcome: &str,
    results: &serde_json::Map<String, Value>,
    intended: &[String],
    pending: &[&String],
) {
    let s = summarize_speed_test(results, intended, pending.len());
    let samples: Vec<&str> = results
        .iter()
        .filter(|(_, v)| !v.as_i64().is_some_and(|ms| ms >= 0))
        .map(|(k, _)| k.as_str())
        .take(5)
        .collect();
    let tail = if samples.is_empty() {
        String::new()
    } else {
        format!("；失败样本 {}", samples.join(", "))
    };
    log::info!(
        "测速一轮完成：outcome={outcome}，请求 {}，成功 {}，超时/失败 {}，未测（让位或中断）{}{tail}",
        intended.len(),
        s.ok,
        s.failed,
        s.absent
    );
}

/// 一轮测速的**终态事件**（[`EVENT_SPEED_TEST_DONE`]）——三条腿各自在**唯一出口**调一次。
///
/// # 为什么放在这里、并且只有一个调用点/腿
///
/// 三条腿（主核池 [`crate::commands::speedtest`]、回退腿、临时核腿）各有 2~4 个 `return`
/// （让位检查点 + 正常收尾）。逐个 `return` 前手动 emit 必然漏 —— 漏掉的那条正是「中断」路径，
/// 而中断恰恰是本事件唯一不可替代的用途。故三条腿一律改成「内核函数照旧多点 return + 薄壳在
/// 唯一出口调本函数」，漏发在结构上写不出来。
///
/// `intended` = 本腿**已裁定要测**的节点 id（波前预筛之后的可测集）。据此派生三个字段：
///  - `total` = `intended.len()`（与该腿进度事件里的 `total` 同一口径，两者失配会让前端的
///    `tested/total` 与终态对不上）；
///  - `tested` = `results.len()`（已出值的，含真实 `-1`）；
///  - `serverIds` = `intended`（本轮原始可测范围，= 中断后「重新测速」的输入）；
///  - `pending` = `intended − results`（**没拿到值**的，= 中断后「继续剩余」的输入）。
///
/// 判据与「差集为什么必须由后端算 / 波前缺席的三类为什么不算 pending」见
/// [`EVENT_SPEED_TEST_DONE`] 的常量文档。
pub fn emit_speed_test_done(
    emit: &mut (dyn FnMut(&str, Value) + Send),
    outcome: &str,
    results: &serde_json::Map<String, Value>,
    intended: &[String],
) {
    // 「缺席即未测」——复用既有诚实性根基（让位未测的节点根本不进 `results`，绝不写假 -1）。
    let pending: Vec<&String> = intended
        .iter()
        .filter(|id| !results.contains_key(id.as_str()))
        .collect();
    log_speed_test_summary(outcome, results, intended, &pending);
    emit(
        EVENT_SPEED_TEST_DONE,
        json!({
            "outcome": outcome,
            "tested": results.len(),
            "total": intended.len(),
            "serverIds": intended,
            "pending": pending,
        }),
    );
}

// ══════════════════════════════════════════════════════════════════════════════
//  生产接线：起临时核 → 就绪门 → 编排 → 无条件收尾。全部 I/O 经注入点，测试用 mock 驱动。
// ══════════════════════════════════════════════════════════════════════════════

/// 临时核会话的注入依赖（生产 [`TempCoreDeps::production`]，测试注入 mock spawner / 假核路径）。
pub struct TempCoreDeps {
    /// 瞬态 sing-box spawn（复用 `tailscale_login_core` 的瞬态核进程抽象）。
    pub spawner: Arc<dyn LoginCoreSpawner>,
    /// spawn 前的 `sing-box check`（fail-fast，复用瞬态登录核那条已建好的抽象）。
    ///
    /// **不是洁癖**：临时核配置里唯一不由本仓完全掌控的部分是 `custom` 协议的**用户原样 JSON**。它形态
    /// 非法时核会预初始化 FATAL、立即退出，而没有这道 check 的话，用户看到的是就绪门那句「10s 内未监听」
    /// —— 把「你那个自定义节点的 JSON 写错了」误报成「网络/端口有问题」，且白等 10 秒。
    pub checker: Arc<dyn ConfigChecker>,
    /// 核二进制解析（生产 = `resolve_core_binary`，与主核**同一份**解析逻辑，禁重复实现）。
    pub resolve_binary: Arc<dyn Fn() -> Result<PathBuf, String> + Send + Sync>,
    /// 临时配置落盘目录（= 主核 config 目录；文件名另用 [`TEMP_CORE_CONFIG_NAME`]，绝不同名）。
    pub config_dir: PathBuf,
    /// 端口分配（生产 = `PortAllocator` + `TokioPortProvider`；测试注入确定性序列）。
    pub allocate_ports: Arc<dyn Fn(usize) -> Vec<u16> + Send + Sync>,
    /// 就绪探测：能连上 `127.0.0.1:<port>` ⇒ 临时核已开始 listen。
    pub probe_port: Arc<dyn Fn(u16) -> bool + Send + Sync>,
    /// 核日志级别（跟随用户配置；诊断态调高时临时核一并抬级，便于复现）。
    pub log_level: String,
    /// 就绪等待上限（生产 [`TEMP_CORE_READY_TIMEOUT_MS`]；测试调小以免 gate 空等一个真实超时）。
    pub ready_timeout_ms: u64,
}

impl TempCoreDeps {
    /// 生产装配：真 spawn + 真核解析 + 真端口分配 + 真 TCP 就绪探测。
    ///
    /// `exclusions` = 用户配置的 control/http/mixed 口 —— **必须排除**，否则临时核占了主核随后要 bind
    /// 的口，用户测完速再点连接就起不来（表现为「测速把代理搞坏了」，归因极难）。
    #[must_use]
    pub fn production(config_dir: PathBuf, exclusions: PortExclusions, log_level: String) -> Self {
        Self {
            spawner: Arc::new(TokioLoginCoreSpawner),
            checker: Arc::new(SingBoxConfigChecker),
            resolve_binary: Arc::new(crate::runtime::proxy::resolve_core_binary),
            config_dir,
            allocate_ports: Arc::new(move |n| {
                PortAllocator::new(TokioPortProvider).resolve_distinct_free_ports(&exclusions, n)
            }),
            probe_port: Arc::new(|port| {
                std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    Duration::from_millis(300),
                )
                .is_ok()
            }),
            log_level,
            ready_timeout_ms: TEMP_CORE_READY_TIMEOUT_MS,
        }
    }
}

/// 一次临时核测速的结局（命令层折成响应信封）。
#[derive(Debug)]
pub enum TempCoreOutcome {
    /// 跑完了（可能部分节点 `-1` = 真实不可测）。`outcome` 同主核路径语义。
    Ran {
        results: serde_json::Map<String, Value>,
        outcome: &'static str,
    },
    /// 起核前/就绪前失败（解析不到核 / 端口分配失败 / 写配置失败 / spawn 失败 / 未就绪）。
    /// **整批一个数值都不产出**（绝不把「核没起来」写成一批 `-1`）。
    Failed(String),
    /// 起核前就已被主核接管 → 一个节点都没测，未测节点缺席。
    Superseded,
}

/// 一次临时核测速会话：起核 → 就绪门 → 编排 → **无条件**收尾（杀核 + 删配置）。
pub struct TempCoreSession;

impl TempCoreSession {
    /// 跑一次临时核测速。
    ///
    /// - `nodes`：[`plan_temp_core`] 裁出的可测节点（保序）；空 → 调用方不该进来（此处防御性返 `Ran` 空）。
    /// - `superseded`：让位判据（生产 = [`is_temp_core_superseded`] 闭包，见模块文档）。
    /// - `measure`：按端口量 warm-TTFB（命令层注入，复用与主核路径**同一个**测量口径 ⇒ 两条腿的数值可比）。
    /// - `emit`：逐节点事件（命令层注入 `AppHandle::emit`）。
    ///
    /// # 收尾纪律
    ///
    /// 杀核 + 删配置走**无条件**路径（正常完成 / 让位 / 就绪失败 / 编排 panic 之外的一切分支共用）——
    /// 漏一条腿的表现是**孤儿 sing-box 常驻**，占着 N 个回环端口且用户完全看不见。
    pub async fn run<Meas, MeasFut>(
        deps: &TempCoreDeps,
        nodes: &[TempNode],
        superseded: &(dyn Fn() -> bool + Sync),
        measure: Meas,
        emit: &mut (dyn FnMut(&str, Value) + Send),
    ) -> TempCoreOutcome
    where
        Meas: Fn(u16) -> MeasFut,
        MeasFut: Future<Output = Option<u32>> + Send + 'static,
    {
        if nodes.is_empty() {
            return TempCoreOutcome::Ran {
                results: serde_json::Map::new(),
                outcome: "completed",
            };
        }
        // ── 让位（起核前）：主核已在跑/已跃迁 → 根本不起临时核（双会话从源头掐掉）──
        if superseded() {
            return TempCoreOutcome::Superseded;
        }

        let binary = match (deps.resolve_binary)() {
            Ok(b) => b,
            Err(e) => return TempCoreOutcome::Failed(e),
        };

        // 端口：整批原子（任一槽拿不到互异空闲口 → 空 vec）。部分池不可用 —— 槽↔端口 1:1 一旦错位，
        // 量到的就是**别的节点**的延迟，比测不了更糟。
        let ports = (deps.allocate_ports)(nodes.len());
        if ports.len() != nodes.len() {
            return TempCoreOutcome::Failed(format!(
                "测速临时核端口分配失败（需 {} 个互异空闲口，实得 {}）",
                nodes.len(),
                ports.len()
            ));
        }

        let config_path = deps.config_dir.join(TEMP_CORE_CONFIG_NAME);
        let cfg = build_temp_core_config(nodes, &ports, &deps.log_level);
        let bytes = match serde_json::to_vec_pretty(&cfg) {
            Ok(b) => b,
            Err(e) => return TempCoreOutcome::Failed(format!("序列化测速临时核配置失败: {e}")),
        };
        if let Err(e) = std::fs::write(&config_path, bytes) {
            return TempCoreOutcome::Failed(format!(
                "写测速临时核配置失败 {}: {e}",
                config_path.display()
            ));
        }

        // `sing-box check` 先验配置形态（fail-fast，同瞬态登录核的既定手法）。没有这道门时，`custom`
        // 协议里用户写错的原样 JSON 会让核预初始化 FATAL ⇒ 用户白等 10s 再看到「未监听」这个指错方向的
        // 报错。check 的诊断原文冒泡给用户 —— 那句话里直接写着哪个字段错了。
        if let Err(e) = deps.checker.check(&binary, &config_path).await {
            remove_temp_config(&config_path);
            return TempCoreOutcome::Failed(e);
        }

        let mut req = SpawnRequest::new(&binary, &config_path);
        // 核输出进日志 sink（非 TTY）；不加 flag 会混入 ANSI 转义。CWD 设可写 config 目录，
        // 理由同主核 spawner（GUI 从 launchd 拉起时父进程 CWD=`/` 只读）。
        req.extra_args = vec!["--disable-color".to_string()];
        req.working_dir = Some(deps.config_dir.clone());
        let child = match deps.spawner.spawn(&req) {
            Ok(c) => c,
            Err(e) => {
                remove_temp_config(&config_path);
                return TempCoreOutcome::Failed(format!("测速临时核 spawn 失败: {e}"));
            }
        };

        // 起核之后的一切分支都必须经收尾（杀核 + 删配置），故从此处起收束到一个 helper。
        let outcome =
            Self::drive_after_spawn(deps, nodes, &ports, superseded, measure, emit, child).await;
        remove_temp_config(&config_path);
        outcome
    }

    /// spawn 之后的编排（就绪门 → 测量 → **无条件杀核**）。抽出以保证「起了核就一定会被杀」这条纪律
    /// 只有一个出口：本函数的每一条 `return` 之前都已 `terminate()`。
    #[allow(clippy::too_many_arguments)]
    async fn drive_after_spawn<Meas, MeasFut>(
        deps: &TempCoreDeps,
        nodes: &[TempNode],
        ports: &[u16],
        superseded: &(dyn Fn() -> bool + Sync),
        measure: Meas,
        emit: &mut (dyn FnMut(&str, Value) + Send),
        mut child: Box<dyn LoginCoreChild>,
    ) -> TempCoreOutcome
    where
        Meas: Fn(u16) -> MeasFut,
        MeasFut: Future<Output = Option<u32>> + Send + 'static,
    {
        let pid = child.pid().unwrap_or(0);
        // 登记进在飞表：应用退出时 `run_exit_cleanup` 据此强杀（本 future 届时不会被 drop，Drop 守卫
        // 覆盖不到那条路径）。守卫在本函数返回/展开时自动注销。
        let _pid_guard = TempCorePidGuard::register(pid);
        log::info!(
            "测速临时核已 spawn：pid={pid}，{} 个节点 / 端口 {:?}",
            nodes.len(),
            ports
        );

        // ── 就绪门（复用 core-supervisor `wait_for_core_ready`；本层只注入真实 I/O）──
        // 就绪信号 = 第一个 HTTP 入站端口可连（对齐 上游 `waitForPortReady(ports[0], 10000)`）。
        let probe = Arc::clone(&deps.probe_port);
        let first_port = ports[0];
        let ready_deps = CoreReadyDeps {
            is_alive: Box::new(move || pid == 0 || pid_alive(pid)),
            is_ready: Box::new(move || {
                let probe = Arc::clone(&probe);
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || probe(first_port))
                        .await
                        .unwrap_or(false)
                })
            }),
            sleep: Box::new(|d| Box::pin(tokio::time::sleep(d))),
            // 就绪等待期同样守让位：临时核起到一半用户点了「连接」⇒ 立刻停等 + 杀核，
            // 而不是先傻等满 10s 再发现要让路（那 10s 里两个核并存）。
            is_superseded: Some(Box::new(superseded)),
            on_retry: None,
        };
        let ready = wait_for_core_ready(
            WaitForCoreReadyOptions {
                timeout_ms: deps.ready_timeout_ms,
                poll_ms: TEMP_CORE_READY_POLL_MS,
            },
            &ready_deps,
        )
        .await;
        match ready {
            CoreReadyOutcome::Ready => {}
            CoreReadyOutcome::Superseded => {
                child.terminate().await;
                return TempCoreOutcome::Superseded;
            }
            other => {
                child.terminate().await;
                // 整批一个数值都不产出：核没起来 ≠ 每个节点都超时。写一批 -1 就是伪造 N 次真实测量。
                return TempCoreOutcome::Failed(format!(
                    "测速临时核未就绪（{other:?}，{}ms 内 127.0.0.1:{first_port} 未监听）",
                    deps.ready_timeout_ms
                ));
            }
        }

        let (results, outcome) = drive_temp_core_measures(
            nodes,
            ports,
            TEMP_CORE_CONCURRENCY,
            superseded,
            measure,
            emit,
        )
        .await;
        child.terminate().await;
        log::info!("测速临时核已回收：pid={pid}，outcome={outcome}");
        TempCoreOutcome::Ran { results, outcome }
    }
}

/// 删临时配置（失败只记日志：删不掉不影响正确性，下次同名覆盖）。
fn remove_temp_config(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("删测速临时核配置失败 {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn env() -> CoreBuildEnv {
        CoreBuildEnv {
            platform: "linux".to_string(),
            arch: "x86_64".to_string(),
            has_cronet: true,
        }
    }

    /// 🔴 **`-1`（真测了没通）与「未测」必须分开计**（陈先生 2026-08-02：「全部测速全部显示 -1，
    /// 跟实际不符」）。两者在日志里混成一类，就再也分不出「网络真挂了」和「本轮压根没测、
    /// 前端把缺席画成了 -1」——而这两件事的修法完全相反。
    ///
    /// **变异锁**：
    /// - 把 `ms >= 0` 写成 `ms > 0` → 第 2 组转红（0ms 是合法的本地极速值，不是失败）；
    /// - 把 `failed` 算成 `results.len()`（不减 ok）→ 第 1 组转红；
    /// - 把 `absent` 并进 `failed` → 第 1 组转红且三分之和溢出（`also_assert_total` 在 debug 下当场炸）。
    #[test]
    fn speed_test_summary_splits_timeout_from_never_measured() {
        let m = |pairs: &[(&str, i64)]| -> serde_json::Map<String, Value> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), json!(v)))
                .collect()
        };
        let intended: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| (*s).into()).collect();

        // 2 个出值（1 成功 1 超时）+ 2 个根本没测。
        let s = summarize_speed_test(&m(&[("a", 120), ("b", -1)]), &intended, 2);
        assert_eq!(
            s,
            SpeedTestSummary {
                ok: 1,
                failed: 1,
                absent: 2
            }
        );

        // 0ms 是合法测量值（本地/极近节点），不是失败。
        let s = summarize_speed_test(&m(&[("a", 0)]), &["a".to_string()], 0);
        assert_eq!(
            s,
            SpeedTestSummary {
                ok: 1,
                failed: 0,
                absent: 0
            }
        );

        // 全员未测（让位/中断）：一个 `-1` 都不该被伪造出来。
        let s = summarize_speed_test(&serde_json::Map::new(), &intended, 4);
        assert_eq!(
            s,
            SpeedTestSummary {
                ok: 0,
                failed: 0,
                absent: 4
            }
        );
    }

    fn srv(id: &str, protocol: Protocol) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: format!("node-{id}"),
            protocol,
            address: "example.com".to_string(),
            port: 443,
            uuid: Some("u-1".to_string()),
            ..Default::default()
        }
    }

    // ══════════════════════════════════════════════════════════════════════════
    // temp_core_tag / plan_temp_core：进核前的裁定。每条盯一个「整批测不成」的面。
    // ══════════════════════════════════════════════════════════════════════════

    /// tag = `out-<id 前 8 位>`（1:1 上游 `:443`）。变异（取全 id / 换前缀）→ 转红：
    /// tag 是入站路由规则与出站的**唯一绑定键**，两侧算法不一致 ⇒ 核里没有匹配出站 → 整批 FATAL。
    #[test]
    fn temp_core_tag_takes_first_eight_chars() {
        assert_eq!(temp_core_tag("0123456789abcdef"), "out-01234567");
        assert_eq!(temp_core_tag("abc"), "out-abc", "短 id 不得 panic / 补位");
    }

    /// **tailscale 一律缺席**：临时核建不出第二个 tsnet 实例，且会与主核抢同一份 tailscale-state。
    ///
    /// **变异锁**：删掉这条腿 → `testable` 多出 ts 节点、`tailscale` 列表空 → 两条断言全红。
    /// 那不是「多测一个」——它会去写主核的登录态目录，把用户已登录的 TS 节点写坏。
    #[test]
    fn plan_excludes_tailscale_nodes() {
        let plan = plan_temp_core(
            &[
                srv("a1111111", Protocol::Vless),
                srv("t1111111", Protocol::Tailscale),
            ],
            &env(),
        );
        assert_eq!(plan.testable.len(), 1);
        assert_eq!(plan.testable[0].id, "a1111111");
        assert_eq!(plan.tailscale, vec!["t1111111".to_string()]);
    }

    /// **naive 缺 cronet → 缺席**（进核会预初始化 FATAL 拖垮**整批**，不是只坏它自己）。
    /// **变异锁**：删掉 `!env.has_cronet` 判据 → naive 节点进 testable → 转红。
    #[test]
    fn plan_excludes_naive_when_cronet_missing() {
        let mut e = env();
        e.has_cronet = false;
        let plan = plan_temp_core(
            &[
                srv("n1111111", Protocol::Naive),
                srv("v1111111", Protocol::Vless),
            ],
            &e,
        );
        assert_eq!(plan.unusable, vec!["n1111111".to_string()]);
        assert_eq!(plan.testable.len(), 1);
    }

    /// cronet 可用时 naive 照常进核（预筛不得误伤正常路径）。
    #[test]
    fn plan_keeps_naive_when_cronet_available() {
        let plan = plan_temp_core(&[srv("n1111111", Protocol::Naive)], &env());
        assert_eq!(plan.testable.len(), 1);
        assert!(plan.unusable.is_empty());
    }

    /// **tag 碰撞消歧**：两个 id 前 8 位相同的节点 → 各拿一个唯一 tag，**都照常进核测**。
    ///
    /// 旧行为是「后来者出局记 `unusable`」。而 id **不保证是 uuid**：手输/导入常见 `mynode-a1` /
    /// `mynode-a2`，前 8 位逐字相同 ⇒ 碰撞是**确定性**的，那个节点于是每次都以笼统的 `notInPool`
    /// 缺席、用户无从修复（他不知道要去改 id 的前 8 位）。
    ///
    /// **变异锁**：① 退回「后来者出局」→ `testable.len()` / `unusable` 两条断言转红；② 干脆不消歧
    /// （两个同 tag 出站）→ tag 互异断言转红，真机则是核启动 FATAL ⇒ **整批**一个都测不成。
    #[test]
    fn plan_disambiguates_colliding_tags_instead_of_dropping_the_node() {
        let plan = plan_temp_core(
            &[
                srv("dup00000-a", Protocol::Vless),
                srv("dup00000-b", Protocol::Vless),
                srv("dup00000-c", Protocol::Vless),
            ],
            &env(),
        );
        assert_eq!(plan.testable.len(), 3, "碰撞不得让任何节点出局");
        assert!(plan.unusable.is_empty(), "碰撞不再是「不可用」");
        let tags: Vec<&str> = plan.testable.iter().map(|n| n.tag.as_str()).collect();
        assert_eq!(
            tags,
            vec!["out-dup00000", "out-dup00000-2", "out-dup00000-3"],
            "碰撞按序号消歧（同 tag 两个出站 ⇒ 核启动 FATAL）"
        );
        // 入站路由键 `in-<tag>` 随之互异 —— 否则两个入站同 tag，同样 FATAL。
        assert_eq!(
            tags.iter().collect::<BTreeSet<_>>().len(),
            3,
            "tag 必须两两互异"
        );
    }

    /// **构造失败 → 缺席**（WG 缺 privateKey）。绝不放半截出站进核。
    /// **变异锁**：把构造失败腿改成「塞个空 outbound 进去」→ `testable` 非空 → 转红。
    #[test]
    fn plan_reports_build_failure_as_unusable() {
        let plan = plan_temp_core(&[srv("w1111111", Protocol::Wireguard)], &env());
        assert!(plan.testable.is_empty(), "缺 wireguardSettings 应构造失败");
        assert_eq!(plan.unusable, vec!["w1111111".to_string()]);
    }

    /// 构造失败后 tag 必须**归还**：否则同 tag 的下一个（能建成的）节点会被误判成碰撞而白白出局。
    /// **变异锁**：删掉 `seen_tags.remove(&tag)` → 第二个节点落进 unusable → 转红。
    #[test]
    fn plan_returns_tag_slot_when_build_failed() {
        let plan = plan_temp_core(
            &[
                srv("dup00000-w", Protocol::Wireguard), // 构造必失败
                srv("dup00000-v", Protocol::Vless),     // 同 tag，但能建成
            ],
            &env(),
        );
        assert_eq!(plan.testable.len(), 1);
        assert_eq!(plan.testable[0].id, "dup00000-v");
    }

    /// 出站里的 `detour` 必须剥掉：链式前置节点的 tag 在临时核里不存在 ⇒ 留着必 FATAL。
    /// **变异锁**：删掉 `obj.remove("detour")` → 断言转红。
    #[test]
    fn plan_strips_detour_from_outbound() {
        let mut s = srv("d1111111", Protocol::Vless);
        s.detour = Some("some-other-node".to_string());
        let plan = plan_temp_core(&[s], &env());
        assert_eq!(plan.testable.len(), 1);
        assert!(
            plan.testable[0].node.get("detour").is_none(),
            "detour 指向临时核里不存在的 tag ⇒ 核启动 FATAL ⇒ 整批测不成"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // build_temp_core_config：临时核配置形状。每条盯一个「核起不来 / 数值属于别人」的面。
    // ══════════════════════════════════════════════════════════════════════════

    fn plain_nodes() -> Vec<TempNode> {
        plan_temp_core(
            &[
                srv("a1111111", Protocol::Vless),
                srv("b1111111", Protocol::Trojan),
            ],
            &env(),
        )
        .testable
    }

    /// **入站↔端口↔出站三者逐位 1:1**。这是本模块最致命的不变式：错位一格 ⇒ 量到的是**别的节点**的
    /// 延迟并挂在这个节点名下（失真数值，比测不了更糟）。
    ///
    /// **变异锁**：把 `zip` 换成对 ports 的独立索引 / 把 route 规则的 outbound 写成固定 tag → 转红。
    #[test]
    fn config_binds_inbound_port_and_outbound_one_to_one() {
        let nodes = plain_nodes();
        let cfg = build_temp_core_config(&nodes, &[20001, 20002], "warn");
        let inbounds = cfg["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 2);
        assert_eq!(inbounds[0]["listen_port"], json!(20001));
        assert_eq!(inbounds[1]["listen_port"], json!(20002));
        assert_eq!(inbounds[0]["listen"], json!("127.0.0.1"), "只许监听回环");
        let rules = cfg["route"]["rules"].as_array().unwrap();
        for (i, node) in nodes.iter().enumerate() {
            assert_eq!(rules[i]["outbound"], json!(node.tag));
            assert_eq!(rules[i]["inbound"][0], json!(format!("in-{}", node.tag)));
        }
    }

    /// 必有 `direct` 出站（sing-box 启动要求）+ `default_domain_resolver` 指向 dns-direct。
    /// **变异锁**：删掉任一 → 核启动 FATAL / 节点域名解析不了 → 整批 -1。
    #[test]
    fn config_has_direct_outbound_and_default_resolver() {
        let cfg = build_temp_core_config(&plain_nodes(), &[20001, 20002], "warn");
        let outbounds = cfg["outbounds"].as_array().unwrap();
        assert!(outbounds.iter().any(|o| o["type"] == json!("direct")));
        assert_eq!(cfg["route"]["default_domain_resolver"], json!("dns-direct"));
        assert_eq!(cfg["dns"]["servers"][0]["tag"], json!("dns-direct"));
    }

    /// **endpoint 腿的 VPN 客户端必须进 `endpoints[]`** —— 塞进 `outbounds[]` 内核 decode 阶段判
    /// `unknown outbound type`，**整个临时核起不来**，同批被测的其它节点一并测不成。
    ///
    /// 判据故意走 `plan_temp_core` 全链路而不是直接构造 `TempNode`：缺陷就在 `build_temp_node`
    /// 里那一行判据上，手搓 `TempNode { is_endpoint: true }` 的测试恰好绕开它（既有几条端点测试
    /// 全是那么写的，所以这个缺陷一条都没红）。
    ///
    /// **变异锁**：把 `build_temp_node` 的判据换回只认 WG/TS 的 `is_mesh_protocol` ⇒ 本条转红。
    #[test]
    fn endpoint_leg_vpn_clients_go_into_endpoints_not_outbounds() {
        use polaris_config_engine::user_config::protocol_settings::{
            OpenconnectSettings, OpenvpnClientSettings, OpenvpnTlsSettings,
        };
        let servers = vec![
            ServerConfig {
                id: "oc111111".into(),
                name: "OC".into(),
                protocol: Protocol::Openconnect,
                openconnect_settings: Some(OpenconnectSettings {
                    server: Some("vpn.example.com:443".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ServerConfig {
                id: "ov111111".into(),
                name: "OV".into(),
                protocol: Protocol::OpenvpnClient,
                openvpn_client_settings: Some(OpenvpnClientSettings {
                    server: Some("vpn.example.com".into()),
                    server_port: Some(1194),
                    tls: Some(OpenvpnTlsSettings::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        let plan = plan_temp_core(&servers, &env());
        assert_eq!(plan.testable.len(), 2, "两个节点都该可测");
        for n in &plan.testable {
            assert!(
                n.is_endpoint,
                "{} 没被判成 endpoint 腿 —— 它会被塞进临时核的 outbounds[]",
                n.tag
            );
        }
        let cfg = build_temp_core_config(&plan.testable, &[20001, 20002], "warn");
        assert_eq!(
            cfg["endpoints"].as_array().map(Vec::len),
            Some(2),
            "两个 VPN 客户端都必须落 endpoints[]"
        );
        let ob_types: Vec<&str> = cfg["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["type"].as_str())
            .collect();
        assert!(
            !ob_types.contains(&"openconnect") && !ob_types.contains(&"openvpn-client"),
            "它们出现在 outbounds[] 里 ⇒ 内核 unknown outbound type，整核起不来。实得 {ob_types:?}"
        );
    }

    /// **纯代理配置零端点噪声**：没有端点节点时不得下发 `endpoints[]` / `dns.rules`
    /// （空数组会让核对 schema 更挑剔，且掩盖「到底有没有端点」这件事）。
    /// **变异锁**：无条件写 `endpoints`/`dns.rules` → 转红。
    #[test]
    fn config_omits_endpoint_sections_for_plain_proxies() {
        let cfg = build_temp_core_config(&plain_nodes(), &[20001, 20002], "warn");
        assert!(cfg.get("endpoints").is_none());
        assert!(cfg["dns"].get("rules").is_none());
    }

    /// **不得引入 sniff / 目标域名的本地解析**（issue #154 的两类解析不变量之一）：
    /// 代理出站的目标域名必须 `ATYP=domain` 透传给出口远程解析，否则所有节点测的是同一条本机解析路径。
    ///
    /// **变异锁**：加 `"sniff": true` 或给 route 规则加 `domain_strategy` → 转红。
    #[test]
    fn config_never_enables_sniff_or_local_target_resolution() {
        let cfg = build_temp_core_config(&plain_nodes(), &[20001, 20002], "warn");
        let raw = serde_json::to_string(&cfg).unwrap();
        assert!(
            !raw.contains("sniff"),
            "sniff 会破坏「目标域名由出口远程解析」不变量"
        );
        assert!(
            !raw.contains("domain_strategy"),
            "针对目标的本地解析会把各节点测成同一条本机路径"
        );
    }

    /// 日志级别透传（诊断态抬级用）。变异（硬编码 warn）→ 转红。
    #[test]
    fn config_passes_through_log_level() {
        let cfg = build_temp_core_config(&plain_nodes(), &[1, 2], "debug");
        assert_eq!(cfg["log"]["level"], json!("debug"));
    }

    /// 端点节点：进 `endpoints[]` + 配一条**按 inbound 键控**的穿隧道 DNS 规则（`disable_cache` 必开）。
    ///
    /// **变异锁**：① 把端点塞进 `outbounds` → `endpoints` 缺失转红；② 删掉 dns.rules → 转红
    /// （端点目标解析回落本机 geo IP，境外出口够不着 → 全批超时）；③ 关掉 `disable_cache` → 转红
    /// （多端点并测时共享缓存互相污染，量到的是别人出口解析出来的 IP）。
    #[test]
    fn config_wires_endpoint_nodes_with_tunneled_dns() {
        let node = TempNode {
            id: "e1111111".to_string(),
            tag: "out-e1111111".to_string(),
            node: json!({ "type": "wireguard", "tag": "out-e1111111" }),
            is_endpoint: true,
            has_local_v6: false,
        };
        let cfg = build_temp_core_config(&[node], &[20001], "warn");
        assert_eq!(cfg["endpoints"].as_array().unwrap().len(), 1);
        // 纯 v4 端点：rules[0] 是 AAAA 抑制（见下一条），route 规则排在它**后面**。
        let rule = &cfg["dns"]["rules"][1];
        assert_eq!(rule["inbound"][0], json!("in-out-e1111111"));
        assert_eq!(rule["server"], json!("dns-exit-out-e1111111"));
        assert_eq!(rule["disable_cache"], json!(true));
        // 穿隧道 DNS server 必须 detour 到本端点 tag（否则查询从本机发，等于没穿隧道）。
        let exit = cfg["dns"]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["tag"] == json!("dns-exit-out-e1111111"))
            .expect("端点必须配自己的穿隧道 DNS server");
        assert_eq!(exit["detour"], json!("out-e1111111"));
    }

    /// 🔴 **纯 v4 端点：AAAA 前置一条 `predefined` 空 NOERROR，且必须排在 route 规则之前**
    /// （旧 legacy `strategy: ipv4_only` 的等价写法，1:1 上游 `0875f66`(#334)）。
    ///
    /// 顺序有牙：DNS 规则先匹配先命中，route 规则是该 inbound 的 catch-all —— 抑制规则排它后面则
    /// AAAA 先被 route 吃掉、抑制**静默失效**，而配置照样通过 `sing-box check`。
    ///
    /// **变异锁**：① 删掉抑制规则 → 长度断言红；② 两条规则顺序颠倒 → 顺序断言红；
    /// ③ 键名写错（`query_types` / `rcode` 拼错）→ 形状断言红。
    #[test]
    fn config_suppresses_aaaa_before_routing_for_v4_only_endpoints() {
        let node = TempNode {
            id: "e1111111".to_string(),
            tag: "out-e1111111".to_string(),
            node: json!({ "type": "wireguard", "tag": "out-e1111111" }),
            is_endpoint: true,
            has_local_v6: false,
        };
        let cfg = build_temp_core_config(&[node], &[20001], "warn");
        let rules = cfg["dns"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2, "纯 v4 端点 = 抑制规则 + route 规则");
        assert_eq!(
            rules[0],
            json!({
                "inbound": ["in-out-e1111111"],
                "query_type": ["AAAA"],
                "action": "predefined",
                "rcode": "NOERROR",
            }),
            "抑制规则形状必须逐字对齐（键名写错 = 静默失效）"
        );
        assert_eq!(
            rules[1]["action"],
            json!("route"),
            "抑制规则必须排在同 inbound 的 route（catch-all）之前，否则 AAAA 先被 route 吃掉"
        );
    }

    /// WG 本地地址含 v6 → **不下发任何族别偏好**（等价旧 `prefer_ipv4`：无顶层 strategy 时内核默认
    /// 并发 A/AAAA 且 v4 排前）。对齐 上游 `:868-877`。
    ///
    /// **变异锁**：把抑制规则无条件下发（丢掉 `!node.has_local_v6` 判据）→ 规则数变 2 → 转红；
    /// 那在真机上的后果是双栈 WG 端点的 v6 解析被砍掉。
    #[test]
    fn config_emits_no_family_preference_for_dual_stack_endpoints() {
        let node = TempNode {
            id: "e2222222".to_string(),
            tag: "out-e2222222".to_string(),
            node: json!({ "type": "wireguard" }),
            is_endpoint: true,
            has_local_v6: true,
        };
        let cfg = build_temp_core_config(&[node], &[20001], "warn");
        let rules = cfg["dns"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "含 v6 的端点只有 route 规则，无抑制规则");
        assert_eq!(rules[0]["action"], json!("route"));
    }

    /// 🔴 **全配置禁 legacy rule-action `strategy`**（sing-box 1.16.0 移除；且与同一份 DNS 配置内任何
    /// 带 `query_type`/`ip_version` 的规则**互斥**，共存即 `initialize dns router` FATAL —— 而
    /// `check` 静默放行，我们起核前那道 check 抓不到）。
    ///
    /// **前置断言防平凡通过**：先证明本配置**确实**含 `query_type`（否则「无 strategy」这条在一个空
    /// 规则集上恒真、门是假的），再断言全文零 `strategy` 且无顶层 `dns.strategy`。
    ///
    /// **变异锁**：把 `"strategy": ...` 写回任一规则 → 转红；偷加顶层 `dns.strategy` → 转红。
    #[test]
    fn temp_core_dns_never_sets_a_legacy_or_top_level_strategy() {
        let node = TempNode {
            id: "e1111111".to_string(),
            tag: "out-e1111111".to_string(),
            node: json!({ "type": "wireguard", "tag": "out-e1111111" }),
            is_endpoint: true,
            has_local_v6: false,
        };
        let cfg = build_temp_core_config(&[node], &[20001], "warn");
        let raw = serde_json::to_string(&cfg["dns"]).unwrap();
        assert!(
            raw.contains("query_type"),
            "前置断言：本配置必须确实带 query_type，否则下面的「禁 strategy」是空集平凡通过"
        );
        assert!(
            !raw.contains("strategy"),
            "legacy rule-action strategy 与 query_type 共存即起核 FATAL：{raw}"
        );
        assert!(
            cfg["dns"].get("strategy").is_none(),
            "顶层 dns.strategy 是「省略 == prefer_ipv4」这条等价性的前提，不得下发"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 让位判据 + 分批。
    // ══════════════════════════════════════════════════════════════════════════

    /// 主核没起、没在起、世代未变 → 未让位（正常临时核测速全程走这条，误判即一个节点都测不成）。
    ///
    /// **反向变异锁**：把判据写成恒真（或多加一条恒真的腿）→ 本测转红。没有这条，下面三条「必要性」
    /// 断言可以被一个 `true` 全部满足。
    #[test]
    fn temp_core_not_superseded_when_main_core_absent() {
        assert!(!is_temp_core_superseded(7, 7, false, false));
    }

    /// 🔴 **第一腿（世代）的必要性**：只有世代跃迁（用户点了连接 / 停止），另两腿都为假。
    ///
    /// 交错窗口：`start` 已 bump 完世代且核已**停**（stop→start 序列 / 起核失败回落）——此刻
    /// `running` 与 `starting` 都可能是 false，唯一可见的证据就是世代变了。
    /// **变异锁**：删掉 `gen_now != gen0` 这条腿 → 本测转红。
    #[test]
    fn temp_core_superseded_on_generation_change_alone() {
        assert!(is_temp_core_superseded(8, 7, false, false));
    }

    /// 🔴 **第二腿（running）的必要性**：主核**已经跑起来了**，而世代与本次基准相同、也不在启动中。
    ///
    /// 交错窗口：bump 发生在本次取 `gen0` **之前**（那一刻 running 还是 false，核在启动中），随后核就绪
    /// ⇒ running 翻真、starting 归假、世代不再动。三腿里只剩这一条能看见主核。
    /// **变异锁**：删掉 `running` 腿 → 本测转红；真机表现是**两个核并存**跑同一批 WG/WARP peer
    /// （上游 G1 的双会话超时事故）。
    #[test]
    fn temp_core_superseded_once_main_core_is_running_alone() {
        assert!(is_temp_core_superseded(7, 7, true, false));
    }

    /// 🔴 **第三腿（starting）的必要性**：主核**正在启动**——世代已 bump 完（⇒ 与本次 `gen0` 相同）、
    /// 核尚未就绪（⇒ `running == false`）。前两腿在这一整段里**同时**为假。
    ///
    /// 窗口有多宽：`ProxyRuntime::start` 的顺序是 `start_inflight+1`（`starting` 的源）→ **stale 清扫
    /// （真机可达数秒）** → `bump_generation` → spawn → 就绪门（最长 10s 级）。用户点「连接」后紧接点
    /// 测速（或托盘/另一窗口点——UI 灰态拦不住跨窗）就确定性落在这段里。
    ///
    /// **变异锁**：删掉 `starting` 腿 → 本测转红；真机表现有两层：① 临时核与启动中的主核同 peer 双会话
    /// 踢线；② 临时核端口只排除 control/http/mixed，会抢走主核刚解析、尚未 bind 的 api/update-in/probe
    /// 池口 ⇒ 主核起核 FATAL address-in-use（用户看到的是「连接失败」）。
    #[test]
    fn temp_core_superseded_while_main_core_is_starting_alone() {
        assert!(is_temp_core_superseded(7, 7, false, true));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // drive_temp_core_measures：滑动窗口调度 + 让位三检查点（全注入，无进程无网络）。
    //
    // 此前这里还有 `plan_temp_batches`（纯逻辑切批）的两条单测。批屏障换成滑动窗口后**没有批这个
    // 概念了**，那个函数与它的两条测试一并删除 —— 不是放宽，是把断言下移到真正的调度器上：
    //  · 「N/limit 切批」→ 由 `never_exceeds_the_concurrency_limit`（真实在飞峰值 ≤ 上限）替代，
    //    这条比切批断言强：它测的是实际并发，而切批只测了一个不再被消费的纯函数；
    //  · 「limit==0 退化成 1、绝不吞掉节点」→ 由 `zero_concurrency_degrades_to_serial_not_to_nothing`
    //    保留同名同义，只是从「测 plan 的返回值」改成「测 drive 真的把每个节点都测了」。
    // ══════════════════════════════════════════════════════════════════════════

    /// 按「第几次询问」脚本化让位信号（0 = 从不让位）。
    fn superseded_at(trip: usize) -> impl Fn() -> bool {
        let calls = AtomicUsize::new(0);
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            trip != 0 && n >= trip
        }
    }

    fn three_nodes() -> Vec<TempNode> {
        ["a1111111", "b1111111", "c1111111"]
            .iter()
            .map(|id| TempNode {
                id: (*id).to_string(),
                tag: temp_core_tag(id),
                node: json!({}),
                is_endpoint: false,
                has_local_v6: false,
            })
            .collect()
    }

    /// 全程未让位 → 全部节点有结果 + `completed` + 每节点恰一条 result/progress。
    /// 这是「让位检查不得误伤正常路径」的基准。
    #[tokio::test]
    async fn measures_all_nodes_when_never_superseded() {
        let mut events: Vec<String> = Vec::new();
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            2,
            &superseded_at(0),
            |_| async { Some(120_u32) },
            &mut |ev, _| events.push(ev.to_string()),
        )
        .await;
        assert_eq!(outcome, "completed");
        assert_eq!(results.len(), 3);
        assert_eq!(results["a1111111"], json!(120));
        assert_eq!(
            events
                .iter()
                .filter(|e| *e == EVENT_SPEED_TEST_RESULT)
                .count(),
            3
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| *e == EVENT_SPEED_TEST_PROGRESS)
                .count(),
            3
        );
    }

    /// **真实超时仍记 -1**（测不通是真的）→ 让位检查不得把它吞成缺席。
    /// 与下一条成对：把「真实 -1」与「让位缺席」钉成两种结局，正是本腿诚实性的全部意义。
    #[tokio::test]
    async fn genuine_timeout_is_recorded_as_minus_one() {
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            8,
            &superseded_at(0),
            |_| async { None },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(outcome, "completed");
        assert_eq!(results["a1111111"], json!(-1));
    }

    /// 让位①（发新活之前）：第 1 次询问即让位 → 一个节点都不测、零**逐节点**事件、`interrupted`。
    ///
    /// # 断言从「零事件」改成「零逐节点事件 + 恰一条终态事件」的理由
    ///
    /// 本条原文是 `events.is_empty()`。终态事件（2026-07-31 B 批）落地后，**中断路径恰恰必须发一条**
    /// —— 原断言留着等于禁止本批的核心行为。守的那条诚实性根基不变：逐节点 result/progress 一条不许有。
    /// 顺带钉住载荷：一个都没测 ⇒ `pending` 必须是**全集**（这也是续测的输入）。
    #[tokio::test]
    async fn interrupts_before_dispatching_without_measuring() {
        let mut events: Vec<(String, Value)> = Vec::new();
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            8,
            &superseded_at(1),
            |_| async { Some(120_u32) },
            &mut |ev, payload| events.push((ev.to_string(), payload)),
        )
        .await;
        assert_eq!(outcome, "interrupted");
        assert!(results.is_empty(), "让位下未测节点必须缺席，绝不写假 -1");
        assert!(
            events
                .iter()
                .all(|(ev, _)| ev != EVENT_SPEED_TEST_RESULT && ev != EVENT_SPEED_TEST_PROGRESS),
            "让位轮不得推逐节点事件：{events:?}"
        );
        let done: Vec<&Value> = events
            .iter()
            .filter(|(ev, _)| ev == EVENT_SPEED_TEST_DONE)
            .map(|(_, p)| p)
            .collect();
        assert_eq!(done.len(), 1, "中断也必须**恰好**发一条终态事件");
        assert_eq!(done[0]["outcome"], json!("interrupted"));
        assert_eq!(done[0]["tested"], json!(0));
        assert_eq!(
            done[0]["serverIds"],
            json!(["a1111111", "b1111111", "c1111111"]),
            "重新测速必须拿到本轮原始范围"
        );
        assert_eq!(
            done[0]["pending"],
            json!(["a1111111", "b1111111", "c1111111"]),
            "一个都没测 ⇒ pending 必须是全集"
        );
    }

    /// 让位②（测量后）：在飞期间主核起来 → 丢弃在飞值（它与主核抢同一条 peer 会话，数值不可信）。
    ///
    /// **变异锁**：删掉这道检查 → 那批值被写进 results、outcome 变 completed → 两条断言全红。
    /// 最危险的假绿形态：双会话下测量多半失败，`None → -1` 恰好「看起来很合理」。
    #[tokio::test]
    async fn discards_in_flight_values_when_main_core_arrives() {
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            8,
            &superseded_at(2), // 批首过、测量后命中
            |_| async { Some(999_u32) },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(outcome, "interrupted");
        assert!(results.is_empty(), "跨核在飞值必须丢弃，不得写入结果集");
    }

    /// 前两个节点正常、第三个之前让位 → 已测部分**保留**，未测缺席。中断 ≠ 丢弃已拿到的真值。
    ///
    /// **trip 编号随调度形态变化（不是放宽门槛）**：3 节点 / 窗口 2 的询问序列是
    /// `发活①(补 a,b) → 节点a → 发活②(补 c) → 节点b → 节点c`，第 5 次落在**节点 c 测完那一刻**
    /// ⇒ c 的值被丢弃、a/b 保留。命中语义与改前逐字相同：先测完的两个留下，第三个缺席。
    #[tokio::test]
    async fn keeps_measured_prefix_on_later_interruption() {
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            2,
            &superseded_at(5),
            |_| async { Some(120_u32) },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(outcome, "interrupted");
        assert_eq!(results.len(), 2, "先测完的两个节点应保留");
        assert!(!results.contains_key("c1111111"), "最后一个未落账 → 缺席");
    }

    /// 🔴 **worker 池（滑动窗口）而非批屏障**：一个慢节点只占住 1/K 的算力，绝不把整批钉死。
    ///
    /// 这是 S2 的**收益本体**。批屏障下 `Σ 每批最大值` —— 一个 8s 的死节点让同批 15 个健康节点也等
    /// 8s；worker 池下界是 `max(单点最坏, 总功/K)`，死节点只堵一个槽。f=0.2/K=16 时「每批至少一个
    /// 死节点」的概率是 0.97 ⇒ 几乎每批都被封顶，两者相差 W=⌈N/K⌉ 倍。
    ///
    /// 构造：窗口 2，节点① 慢（400ms），节点②③④ 秒回。滑动窗口下 ②③④ 全部在 ① 之前回来；
    /// 批屏障下 ③④ 属第二批，必须等 ① 收尾 ⇒ 落在 ① 之后。
    /// **变异锁**：改回 `plan_temp_batches` 批屏障 → `slow-done` 跑到 ③④ 前面 → 转红。
    #[tokio::test]
    async fn a_slow_node_does_not_block_the_rest_of_the_queue() {
        let nodes: Vec<TempNode> = ["s1111111", "f2222222", "f3333333", "f4444444"]
            .iter()
            .map(|id| TempNode {
                id: (*id).to_string(),
                tag: temp_core_tag(id),
                node: json!({}),
                is_endpoint: false,
                has_local_v6: false,
            })
            .collect();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let mlog = Arc::clone(&log);
        let elog = Arc::clone(&log);
        let (results, outcome) = drive_temp_core_measures(
            &nodes,
            &[1, 2, 3, 4],
            2, // 窗口 2：慢节点占住一个槽，另一个槽必须继续轮转
            &superseded_at(0),
            move |port| {
                let mlog = Arc::clone(&mlog);
                async move {
                    if port == 1 {
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        mlog.lock().unwrap().push("slow-done".to_string());
                    }
                    Some(120_u32)
                }
            },
            &mut |ev, payload| {
                if ev == EVENT_SPEED_TEST_RESULT {
                    let id = payload["serverId"].as_str().unwrap().to_string();
                    elog.lock().unwrap().push(format!("emit:{id}"));
                }
            },
        )
        .await;

        assert_eq!(outcome, "completed");
        assert_eq!(results.len(), 4, "四个节点全部要有结果");
        let log = log.lock().unwrap();
        let slow = log
            .iter()
            .position(|l| l == "slow-done")
            .expect("慢节点必须测完");
        for id in ["f3333333", "f4444444"] {
            let fast = log
                .iter()
                .position(|l| *l == format!("emit:{id}"))
                .unwrap_or_else(|| panic!("{id} 必须回填"));
            assert!(
                fast < slow,
                "队尾的健康节点必须在慢节点之前测完（批屏障会让它等满一整批）：{log:?}"
            );
        }
    }

    /// 🔴 **在飞并发不得超过窗口上限**：不设上限时大订阅会把 N 路 TLS/QUIC 握手同时打出去
    /// → 本机 CPU/连接数打满 → 一批**假超时**（节点其实是好的）。
    ///
    /// **变异锁**：把补位条件里的 `set.len() < window` 去掉（一次性全 spawn）→ 峰值 6 > 2 → 转红。
    #[tokio::test]
    async fn never_exceeds_the_concurrency_limit() {
        let nodes: Vec<TempNode> = ["n1111111", "n2222222", "n3333333", "n4444444", "n5555555"]
            .iter()
            .map(|id| TempNode {
                id: (*id).to_string(),
                tag: temp_core_tag(id),
                node: json!({}),
                is_endpoint: false,
                has_local_v6: false,
            })
            .collect();
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (m_live, m_peak) = (Arc::clone(&live), Arc::clone(&peak));
        let (results, outcome) = drive_temp_core_measures(
            &nodes,
            &[1, 2, 3, 4, 5],
            2,
            &superseded_at(0),
            move |_| {
                let (live, peak) = (Arc::clone(&m_live), Arc::clone(&m_peak));
                async move {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                    Some(120_u32)
                }
            },
            &mut |_, _| {},
        )
        .await;

        assert_eq!(outcome, "completed");
        assert_eq!(results.len(), 5, "全部节点都要测到");
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "在飞峰值 {} 超过窗口上限 2",
            peak.load(Ordering::SeqCst)
        );
    }

    /// `concurrency == 0` 必须退化成 1，**绝不一个都不测**（零事件 ⇒ 前端测速按钮永久卡灰）。
    /// **变异锁**：去掉 `.max(1)` → 窗口恒 0、一个都不 spawn、`results` 空 → 转红。
    #[tokio::test]
    async fn zero_concurrency_degrades_to_serial_not_to_nothing() {
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            0,
            &superseded_at(0),
            |port| async move { Some(u32::from(port)) },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(outcome, "completed");
        assert_eq!(results.len(), 3, "0 并发是配置错误，不是「不测」的意思");
        assert_eq!(results["a1111111"], json!(1), "串行也不得让结果与端口错位");
    }

    /// 🔴 **让位②（在飞轮询）：supersede 命中即 `abort_all` + 立刻返回，不等在飞测量收尾。**
    ///
    /// **本腿守的是真事故面**：窗口里的节点全部不可达时，「发新活之前」与「每节点测完」两个检查点
    /// 一个都醒不过来 —— 信号出现后临时核（**及其已建立的 WG/WARP 会话**）还要活满一整个测量超时，
    /// 与启动中的主核同 peer 双会话踢线、并抢主核尚未 bind 的端口。Linux/macOS 靠主核 `start()` 入口
    /// 的 stale sweep 顺带杀掉——那是副作用缓解、不是设计保证；Windows 无 sweep
    /// （`scan_running_cores` 恒返空）⇒ 全程重叠。
    ///
    /// 牙：删掉 `Err(_elapsed)` 那条轮询臂（或把 `timeout(poll, join_next())` 换回裸 `join_next()`）
    /// → 本测的**时限**断言转红：注入的测量要 30s 才结束，而本测只给 5s。
    #[tokio::test]
    async fn aborts_in_flight_measurements_instead_of_waiting_for_them() {
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            drive_temp_core_measures(
                &three_nodes(),
                &[1, 2, 3],
                8,
                // 发活（第 1 次询问）放行 → 全部进入在飞；在飞轮询（第 2 次）命中。
                &superseded_at(2),
                // 测量 30s 不返回：只有真 abort 才能让本函数在 5s 内收场。
                |_| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Some(120_u32)
                },
                &mut |_, _| {},
            ),
        )
        .await
        .expect("在飞让位必须中断在飞测量：等它收尾 = 临时核与主核重叠一整个测量超时");
        let (results, outcome) = out;
        assert_eq!(outcome, "interrupted");
        assert!(
            results.is_empty(),
            "被中断的在飞测量必须缺席，绝不补 -1（让位未测 ≠ 真实超时）"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "必须在轮询间隔量级内返回，而不是等满 30s 的在飞测量"
        );
    }

    /// 在飞轮询**不得误伤正常路径**：从不让位时，全部节点照常测完（轮询只是旁路）。
    #[tokio::test]
    async fn in_flight_polling_does_not_disturb_slow_but_uninterrupted_measurements() {
        let (results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            8,
            &superseded_at(0),
            // 比轮询间隔长 → 至少触发一次轮询，且必须不影响结果。
            |port| async move {
                tokio::time::sleep(Duration::from_millis(TEMP_CORE_SUPERSEDE_POLL_MS + 120)).await;
                Some(u32::from(port))
            },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(outcome, "completed");
        assert_eq!(results.len(), 3);
        assert_eq!(results["a1111111"], json!(1), "轮询不得让结果与端口错位");
    }

    /// 🔴 **逐节点回填**：先测完的节点必须在**其它节点还在飞**的时候就上屏。
    ///
    /// 按批统一回填时，首个延迟数字要等整批最慢的那个（一批里有一个死节点就是一个完整超时），
    /// 屏幕先空十几秒。总耗时一点没变，主观耗时天差地别（差异分析 R3）。
    ///
    /// **变异锁**：改回「先 drain 完整批 → 收集循环统一 emit」→ `emit:a1111111` 落到 `c-measured`
    /// 之后 → 转红。
    #[tokio::test]
    async fn reports_each_node_as_soon_as_it_finishes() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let mlog = Arc::clone(&log);
        let elog = Arc::clone(&log);
        let (_results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            8,
            &superseded_at(0),
            move |port| {
                let mlog = Arc::clone(&mlog);
                async move {
                    // 第三个节点慢：它还没回来时，第一个节点的结果就必须已经推出去了。
                    if port == 3 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        mlog.lock().unwrap().push("c-measured".to_string());
                    }
                    Some(120_u32)
                }
            },
            &mut |ev, payload| {
                if ev == EVENT_SPEED_TEST_RESULT {
                    let id = payload["serverId"].as_str().unwrap().to_string();
                    elog.lock().unwrap().push(format!("emit:{id}"));
                }
            },
        )
        .await;

        assert_eq!(outcome, "completed");
        let log = log.lock().unwrap();
        let emit_a = log
            .iter()
            .position(|l| l == "emit:a1111111")
            .expect("首个节点必须回填");
        let c_done = log
            .iter()
            .position(|l| l == "c-measured")
            .expect("慢节点必须测完");
        assert!(
            emit_a < c_done,
            "首个节点的结果必须在慢节点回来之前就上屏（实际顺序：{log:?}）"
        );
    }

    /// 🔴 **进度计数恒单调**：`tested` 严格 1,2,…,N，`ok` 非降。
    ///
    /// 前端 `NodesScreen` 靠 `tested >= total` 复位测速灰态 —— 计数一旦回退或跳号，要么按钮永久卡灰，
    /// 要么进度条倒着走。**变异锁**：把 `tested` 改成按批内下标计算（或在 emit 之后才自增）→ 转红。
    #[tokio::test]
    async fn progress_counter_is_strictly_monotonic() {
        let mut tested_seq: Vec<i64> = Vec::new();
        let mut ok_seq: Vec<i64> = Vec::new();
        let (_results, outcome) = drive_temp_core_measures(
            &three_nodes(),
            &[1, 2, 3],
            2, // 跨批也必须连续计数
            &superseded_at(0),
            |port| async move {
                if port == 1 {
                    None // 真实超时 → -1，不计入 ok
                } else {
                    Some(120_u32)
                }
            },
            &mut |ev, payload| {
                if ev == EVENT_SPEED_TEST_PROGRESS {
                    tested_seq.push(payload["tested"].as_i64().unwrap());
                    ok_seq.push(payload["ok"].as_i64().unwrap());
                }
            },
        )
        .await;

        assert_eq!(outcome, "completed");
        assert_eq!(tested_seq, vec![1, 2, 3], "tested 必须严格递增且不跳号");
        assert!(
            ok_seq.windows(2).all(|w| w[1] >= w[0]),
            "ok 必须非降：{ok_seq:?}"
        );
    }

    /// **端口按节点索引取**：并发乱序回收不得让结果与端口错位。
    /// 注入「端口 → 延迟」的一一映射，断言每个节点拿到的正是**自己**那个端口的值。
    ///
    /// **变异锁**：把 `measure(ports[*i])` 换成 `ports[0]` / 用批内序号取端口 → 转红。
    /// 这条盯的是本模块最贵的失真面：数值属于别的节点。
    #[tokio::test]
    async fn each_node_measures_through_its_own_port() {
        let (results, _) = drive_temp_core_measures(
            &three_nodes(),
            &[10, 20, 30],
            8,
            &superseded_at(0),
            |port| async move { Some(u32::from(port)) },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(results["a1111111"], json!(10));
        assert_eq!(results["b1111111"], json!(20));
        assert_eq!(results["c1111111"], json!(30));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // TempCoreSession：起核 → 就绪门 → 编排 → **无条件收尾**（mock spawner，无真进程）。
    // ══════════════════════════════════════════════════════════════════════════

    /// 假瞬态核：记录 terminate 次数，永不真起进程。
    struct FakeChild {
        terminated: Arc<AtomicUsize>,
        /// 假 pid（`None` = 取不到 → 就绪门 is_alive 恒真）。仅退出清理登记表那条测试用。
        pid: Option<u32>,
    }

    #[async_trait]
    impl LoginCoreChild for FakeChild {
        fn pid(&self) -> Option<u32> {
            // 默认 None → pid=0 → 就绪门的 is_alive 恒真（假核不死），把判定压到 is_ready 那条腿上
            self.pid
        }
        fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>> {
            None
        }
        fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>> {
            None
        }
        async fn wait(&mut self) {}
        async fn terminate(&mut self) {
            self.terminated.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeSpawner {
        terminated: Arc<AtomicUsize>,
        spawns: Arc<AtomicUsize>,
        fail: bool,
        child_pid: Option<u32>,
    }

    impl LoginCoreSpawner for FakeSpawner {
        fn spawn(
            &self,
            _req: &SpawnRequest,
        ) -> Result<Box<dyn LoginCoreChild>, polaris_core_supervisor::SpawnError> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(polaris_core_supervisor::SpawnError::Spawn {
                    bin: PathBuf::from("/nonexistent"),
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                });
            }
            Ok(Box::new(FakeChild {
                terminated: Arc::clone(&self.terminated),
                pid: self.child_pid,
            }))
        }
    }

    /// 假 `sing-box check`：`ok=false` 模拟核判定配置无效（**绝不**真起 sing-box）。
    struct FakeChecker {
        ok: bool,
    }

    #[async_trait]
    impl ConfigChecker for FakeChecker {
        async fn check(
            &self,
            _binary: &std::path::Path,
            _config: &std::path::Path,
        ) -> Result<(), String> {
            if self.ok {
                Ok(())
            } else {
                Err("sing-box check 判定测速配置无效: bad custom outbound".to_string())
            }
        }
    }

    struct Harness {
        deps: TempCoreDeps,
        terminated: Arc<AtomicUsize>,
        spawns: Arc<AtomicUsize>,
        dir: PathBuf,
    }

    /// 造一个全 mock 的会话依赖：假 spawner + 假 check + 假核路径 + 确定性端口 + 可控就绪。
    /// **零真进程、零网络**（`probe_port` 是纯闭包，绝不 connect；`checker` 不 exec 任何东西）。
    fn harness(ready: bool, spawn_fail: bool, ports: Vec<u16>) -> Harness {
        harness_with_pid(ready, spawn_fail, ports, None)
    }

    /// 同 [`harness`]，但给假核一个 pid（退出清理登记表那条测试用；其余一律 `None`）。
    fn harness_with_pid(
        ready: bool,
        spawn_fail: bool,
        ports: Vec<u16>,
        child_pid: Option<u32>,
    ) -> Harness {
        let dir = std::env::temp_dir().join(format!(
            "polaris-tempcore-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let terminated = Arc::new(AtomicUsize::new(0));
        let spawns = Arc::new(AtomicUsize::new(0));
        let fake_bin = dir.join("fake-sing-box");
        std::fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();
        Harness {
            deps: TempCoreDeps {
                spawner: Arc::new(FakeSpawner {
                    terminated: Arc::clone(&terminated),
                    spawns: Arc::clone(&spawns),
                    fail: spawn_fail,
                    child_pid,
                }),
                checker: Arc::new(FakeChecker { ok: true }),
                resolve_binary: Arc::new(move || Ok(fake_bin.clone())),
                config_dir: dir.clone(),
                allocate_ports: Arc::new(move |n| ports.iter().copied().take(n).collect()),
                probe_port: Arc::new(move |_| ready),
                log_level: "warn".to_string(),
                ready_timeout_ms: 400,
            },
            terminated,
            spawns,
            dir,
        }
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn cleanup(dir: &PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 正常路径：起核 → 就绪 → 测完 → **杀核 + 删配置**。
    ///
    /// **变异锁**：删掉 `child.terminate()` → `terminated == 0` 转红（真机表现 = 孤儿 sing-box 常驻，
    /// 占着 N 个回环端口且用户完全看不见）；删掉 `remove_temp_config` → 配置文件残留断言转红。
    #[tokio::test]
    async fn session_kills_core_and_removes_config_on_success() {
        let h = harness(true, false, vec![20001, 20002, 20003]);
        let nodes = three_nodes();
        let out = TempCoreSession::run(
            &h.deps,
            &nodes,
            &|| false,
            |_| async { Some(50_u32) },
            &mut |_, _| {},
        )
        .await;
        match out {
            TempCoreOutcome::Ran { results, outcome } => {
                assert_eq!(outcome, "completed");
                assert_eq!(results.len(), 3);
            }
            other => panic!("应跑完，得到 {other:?}"),
        }
        assert_eq!(h.terminated.load(Ordering::SeqCst), 1, "临时核必须被杀");
        assert!(
            !h.dir.join(TEMP_CORE_CONFIG_NAME).exists(),
            "临时配置必须删掉（残留会让下次导出诊断报告读到一份不属于任何在跑核的配置）"
        );
        cleanup(&h.dir);
    }

    /// 配置**落的是独立文件**，绝不是主核的 `singbox-runtime.json`。
    ///
    /// **变异锁**：把文件名改成 `singbox-runtime.json` → 转红。那会在主核起来前**覆盖掉主核的运行配置**，
    /// 表现为「测完速再点连接，代理行为莫名其妙」——归因极难。
    #[test]
    fn temp_config_name_is_isolated_from_the_main_core() {
        assert_eq!(TEMP_CORE_CONFIG_NAME, "speedtest-core.json");
        assert_ne!(TEMP_CORE_CONFIG_NAME, "singbox-runtime.json");
    }

    /// 就绪门失败 → 杀核 + **整批一个数值都不产出**（核没起来 ≠ 每个节点都超时）。
    ///
    /// **变异锁**：把未就绪腿改成「给每个节点记 -1」→ 本测转红：那是伪造 N 次真实测量。
    #[tokio::test]
    async fn session_reports_failure_without_faking_results_when_not_ready() {
        let h = harness(false, false, vec![20001, 20002, 20003]);
        let out = TempCoreSession::run(
            &h.deps,
            &three_nodes(),
            &|| false,
            |_| async { Some(50_u32) },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(out, TempCoreOutcome::Failed(_)), "得到 {out:?}");
        assert_eq!(h.terminated.load(Ordering::SeqCst), 1, "未就绪也必须杀核");
        assert!(!h.dir.join(TEMP_CORE_CONFIG_NAME).exists());
        cleanup(&h.dir);
    }

    /// **起核前就让位 → 根本不 spawn**（双会话从源头掐掉，而不是起了再杀）。
    /// **变异锁**：删掉起核前那道检查 → `spawns == 1` 转红。
    #[tokio::test]
    async fn session_never_spawns_when_already_superseded() {
        let h = harness(true, false, vec![20001, 20002, 20003]);
        let out = TempCoreSession::run(
            &h.deps,
            &three_nodes(),
            &|| true,
            |_| async { Some(50_u32) },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(out, TempCoreOutcome::Superseded), "得到 {out:?}");
        assert_eq!(h.spawns.load(Ordering::SeqCst), 0, "让位态绝不许起临时核");
        cleanup(&h.dir);
    }

    /// **端口不够 → 整批失败，绝不部分起核**：槽↔端口 1:1 一旦错位，量到的就是别的节点的延迟。
    /// **变异锁**：把等长断言放宽成 `ports.len() >= 1` → 转红。
    #[tokio::test]
    async fn session_fails_atomically_when_ports_are_short() {
        let h = harness(true, false, vec![20001]); // 只给 1 个，需 3 个
        let out = TempCoreSession::run(
            &h.deps,
            &three_nodes(),
            &|| false,
            |_| async { Some(50_u32) },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(out, TempCoreOutcome::Failed(_)), "得到 {out:?}");
        assert_eq!(h.spawns.load(Ordering::SeqCst), 0, "端口不齐就不该起核");
        cleanup(&h.dir);
    }

    /// **配置形态非法 → 根本不 spawn**，且 check 的诊断**原文冒泡**（那句话里写着哪个字段错了）。
    ///
    /// 唯一不由本仓完全掌控的配置片段是 `custom` 协议的用户原样 JSON。没有这道 fail-fast 门时，用户看到
    /// 的是就绪门那句「10s 内未监听」—— 把「你的自定义节点 JSON 写错了」误报成「网络/端口有问题」，
    /// 还白等 10 秒。
    ///
    /// **变异锁**：删掉 `deps.checker.check(...)` 那段 → `spawns == 1` + outcome 变成就绪失败 → 转红。
    #[tokio::test]
    async fn session_rejects_invalid_config_before_spawning() {
        let mut h = harness(true, false, vec![20001, 20002, 20003]);
        h.deps.checker = Arc::new(FakeChecker { ok: false });
        let out = TempCoreSession::run(
            &h.deps,
            &three_nodes(),
            &|| false,
            |_| async { Some(50_u32) },
            &mut |_, _| {},
        )
        .await;
        match out {
            TempCoreOutcome::Failed(e) => assert!(
                e.contains("bad custom outbound"),
                "check 的诊断必须原文冒泡（吞成通用文案 = 用户无从知道哪个字段错了），得到：{e}"
            ),
            other => panic!("非法配置应 fail-fast，得到 {other:?}"),
        }
        assert_eq!(h.spawns.load(Ordering::SeqCst), 0, "配置无效就不该起核");
        assert!(!h.dir.join(TEMP_CORE_CONFIG_NAME).exists());
        cleanup(&h.dir);
    }

    /// spawn 失败 → 失败信封 + **临时配置照样删掉**（残留文件会被诊断导出当成在跑核的配置）。
    #[tokio::test]
    async fn session_cleans_up_config_when_spawn_fails() {
        let h = harness(true, true, vec![20001, 20002, 20003]);
        let out = TempCoreSession::run(
            &h.deps,
            &three_nodes(),
            &|| false,
            |_| async { Some(50_u32) },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(out, TempCoreOutcome::Failed(_)), "得到 {out:?}");
        assert!(!h.dir.join(TEMP_CORE_CONFIG_NAME).exists());
        cleanup(&h.dir);
    }

    /// 空节点集 → 不起核、不失败（调用方本不该进来；防御性返 completed 空）。
    #[tokio::test]
    async fn session_is_noop_for_empty_node_set() {
        let h = harness(true, false, vec![]);
        let out = TempCoreSession::run(
            &h.deps,
            &[],
            &|| false,
            |_| async { Some(1_u32) },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(out, TempCoreOutcome::Ran { .. }));
        assert_eq!(h.spawns.load(Ordering::SeqCst), 0);
        cleanup(&h.dir);
    }

    /// 🔵 **结构守卫**：生产装配必须复用主核的 `resolve_core_binary`，绝不另写一份核路径解析。
    ///
    /// 另写一份的失效方式是静默的：换核（core-swap）后主核用新核、临时核仍指旧核路径 ⇒ 测速结果来自
    /// 一个**版本不同**的内核，而两边都「能跑」。
    ///
    /// 🔴 **二次封顶**：`top_level_fn_body` 按**列 0** 的 `\n}\n` 收尾，对 `production` 这种 4 空格缩进
    /// 的方法实际扫到的是 `impl TempCoreDeps` 的结尾。将来在该 impl 里追加任何含 `resolve_core_binary`
    /// 字样的方法，本守卫就会**照绿**（哪怕 `production` 自己已经不再复用它）。故在此把切片再收到
    /// 「下一个同级方法之前」，把判据钉回 `production` **自己的**函数体。
    #[test]
    fn production_deps_reuse_the_main_core_binary_resolver() {
        let src = include_str!("speedtest.rs");
        let body = crate::commands::guard_scan::top_level_fn_body(
            src,
            "    pub fn production(config_dir: PathBuf",
        );
        // 切片以自身签名开头（不含前导换行）⇒ 这里找到的必是**下一个**同级方法。
        let body = body
            .find("\n    pub fn ")
            .map_or(body.as_str(), |i| &body[..i]);
        assert_eq!(
            body.matches("    pub fn ").count(),
            1,
            "二次封顶失效：切片里混进了同 impl 的其它方法 ⇒ 下面的断言可被「删这里、加那里」骗过"
        );
        assert!(
            body.contains("crate::runtime::proxy::resolve_core_binary"),
            "临时核必须与主核共用同一份核二进制解析（另写一份 ⇒ 换核后两边指向不同内核，静默）"
        );
        assert!(
            body.contains("resolve_distinct_free_ports"),
            "端口必须走 core-supervisor 的批分配（自己 bind 一圈会丢掉排除集，撞主核端口）"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 在飞临时核 pid 表（应用退出清理的收口）。**任何一条都不对真实进程发信号**。
    // ══════════════════════════════════════════════════════════════════════════

    /// pid 表是**进程级**共享状态 ⇒ 触碰它的用例必须串行，否则彼此排空对方的登记。
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    fn registry_guard() -> MutexGuard<'static, ()> {
        REGISTRY_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 🔴 会话必须把在飞 pid 登记进表（**退出清理唯一能杀到它的途径**），并在收尾时注销。
    ///
    /// 为什么不能只靠 child 的 `Drop` 守卫：应用退出走 `ExitRequested → run_exit_cleanup → 进程退出`，
    /// 在飞的 tokio task **根本不会被 drop** ⇒ Drop 守卫够不着 ⇒ 留下持有 N 个回环端口 + WG peer 会话
    /// 的孤儿 sing-box（Windows 无 stale sweep 兜底，永不被清）。
    ///
    /// 牙：① 删掉 `TempCorePidGuard::register(pid)` → 在飞断言转红；② 把守卫换成裸 insert（不注销）
    /// → 收尾断言转红（表里留着死 pid，退出时可能误杀一个 pid 复用的无关进程）。
    ///
    /// 用**本进程自己的 pid** 当假核 pid：就绪门的 `is_alive` 需要一个真存活的 pid，而本测**只读**表、
    /// 绝不调用发信号的那条路径（`kill_inflight_temp_cores`）。
    // `await_holding_lock`：`REGISTRY_LOCK` 是**测试串行闸**，语义上就必须罩住整个 async 测试体
    // （否则并发的排空用例会把本测登记的 pid 抽走）。不会死锁：`#[tokio::test]` 默认单线程运行时，
    // 而另一个持锁者是普通同步 `#[test]`（跑在别的线程上），两边各自推进；换 async Mutex 反而要求
    // 同步那条用例也变 async，为一个纯串行闸把射程搞大。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn session_registers_inflight_pid_so_app_exit_cleanup_can_reach_it() {
        let _lock = registry_guard();
        let self_pid = std::process::id();
        let h = harness_with_pid(true, false, vec![20001, 20002, 20003], Some(self_pid));
        let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Arc::clone(&seen);
        let out = TempCoreSession::run(
            &h.deps,
            &three_nodes(),
            &|| false,
            move |_| {
                let probe = Arc::clone(&probe);
                async move {
                    if temp_core_pids().contains(&self_pid) {
                        probe.store(true, Ordering::SeqCst);
                    }
                    Some(50_u32)
                }
            },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(out, TempCoreOutcome::Ran { .. }), "得到 {out:?}");
        assert!(
            seen.load(Ordering::SeqCst),
            "测量在飞期间 pid 必须在表里 —— 否则应用退出时清理路径根本看不见这个核"
        );
        assert!(
            !temp_core_pids().contains(&self_pid),
            "会话收尾必须注销 pid（留着 = 退出时对一个已死 pid 发信号，pid 复用即误杀无关进程）"
        );
        cleanup(&h.dir);
    }

    /// 排空语义：逐 pid 收割一次 + 计数 + **幂等**（第二次调用返 0，绝不重复发信号）。
    ///
    /// 收割动作经注入闭包 ⇒ 零真实信号。假 pid 取 `> i32::MAX`：即便有人把它接到真 `send_signal` 上，
    /// `checked_pid` 也会挡掉（负数 pid 是 kill 的**广播**语义 —— 那是全场 SIGKILL）。
    #[test]
    fn kill_inflight_temp_cores_drains_table_once_and_counts_each_pid() {
        let _lock = registry_guard();
        let fake: u32 = 0xDEAD_BEEF;
        temp_core_pids().insert(fake);
        let mut killed: Vec<u32> = Vec::new();
        let n = kill_temp_cores_with(|pid| killed.push(pid));
        assert_eq!(n, killed.len(), "返回值必须等于实际收割条数");
        assert!(killed.contains(&fake), "在飞 pid 必须被收割");
        // 幂等：表已排空 → 再调零收割（重复发信号 = 对复用了该 pid 的无关进程动手）。
        let mut again: Vec<u32> = Vec::new();
        assert_eq!(kill_temp_cores_with(|pid| again.push(pid)), 0);
        assert!(again.is_empty());
    }

    /// 🔵 **调用点守卫**：`main.rs` 的退出清理必须真的调 [`kill_inflight_temp_cores`]。
    ///
    /// 没有这条，「登记了 pid」与「退出时会被杀」之间是断的，而断了的表现**恰好是静默的**：
    /// 用户看不到孤儿核，只在下次起核时莫名 address-in-use（Windows 连那次兜底都没有）。
    /// 牙：把 `main.rs::run_exit_cleanup` 里那行删掉 / 挪出该函数 → 转红。
    #[test]
    fn app_exit_cleanup_kills_inflight_temp_cores() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            include_str!("../main.rs"),
            "fn run_exit_cleanup(",
        );
        assert!(
            body.contains("kill_inflight_temp_cores()"),
            "退出清理必须收掉在飞测速临时核：它不在 ProxyRuntime 的任何生命周期槽里，proxy.stop() 碰不到它"
        );
    }
}
