//! generateSingBoxConfig 编排逻辑 —— 上游 `ProxyManager.generateSingBoxConfig`（L3470-3637）1:1 移植。
//!
//! 装配六 builder（log/dns/inbounds/outbounds/route）+ experimental.cache_file + 1.14 services
//! （management API / dashboard）。纯函数 + 依赖注入：Polaris 所有 `this.*` 实例态经 `GenerateConfigDeps`
//! 注入（raceServerPort / probe 端口 / lanResolverForDns / hasCronet / hasManagementApi / FS 路径 …）。
//!
//! 装配顺序（Polaris 时序严格保持）：
//!  1. withRaceOff：raceServerPort==0 → clone config 清 dnsConfig.resolveNodeDomainsAhead。
//!  2. selectedServer 校验：isDirect 跳过；naive 缺 libcronet → Err。
//!  3. buildOutbounds（先）→ 产 pendingEndpoints / pendingRuleSelectors（route/dns 消费）。
//!  4. buildLogConfig / buildDnsConfig / buildInbounds / buildRouteConfig（消费 pendingEndpoints）。
//!  5. 装配顶层 SingBoxConfig（log/dns/inbounds/outbounds/route/experimental.cache_file）。
//!  6. endpoints 注入顶层（pendingEndpoints 非空）。
//!  7. services 注入（has_management_api 门控：api service + 可选 dashboard）。
//!  8. fixRouteDeadReferences（route 死引用兜底）。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use polaris_helper_proto::Platform;
use serde::Serialize;

use crate::builder::dns::{build_dns_config, DnsConfigDeps};
use crate::builder::endpoint_routes::mesh_system_supported_on_platform;
use crate::builder::helpers::{build_id_to_tag_map, ServerLike};
use crate::builder::inbounds::{build_inbounds, InboundsDeps};
use crate::builder::log::{build_log_config, LogBuildDeps, LogConfigInput};
use crate::builder::orchestration::fix_route_dead_references;
use crate::builder::outbounds::{build_outbounds, OutboundsDeps};
use crate::builder::route::{build_route_config_with_report, RouteConfigDeps};
use crate::singbox::{
    ApiDashboard, ApiService, CacheFile, Experimental, HttpClient, SingBoxConfig,
};
use crate::user_config::app_config::UserConfig;
use crate::user_config::dns_constants::is_sentinel_selection;
use crate::user_config::log_level::LogLevel;
use crate::user_config::proxy_mode::ProxyModeType;
use crate::user_config::server_config::Protocol;

/// cache_id 品牌归一化值（§D.2）：上游 用 上游的 dns cache_id，Polaris 改 'polaris-dns-v2'。
///
/// `store_dns` 把 DNS 应答持久化，bump 本值令旧条目不可达（逻辑清库）。
///
/// # 射程边界：**bump 它对 `store_fakeip` 无效**
///
/// 这里原本写的是「store_dns/store_fakeip 把投毒条目持久化，bump cache_id 令旧条目不可达」，
/// 从 上游 逐字继承（那边同样的话写在 `ProxyManager.ts` 的 cache_id bump 注释里）。**对 fakeip
/// 那半句不成立**：内核的 `experimental/cachefile/fakeip.go` 全程直操作 `fakeip_address` /
/// `fakeip_domain4` / `fakeip_domain6` 三个**顶层 bucket**，不经 `cacheID` 命名空间；`cache.go` 的
/// 前缀白名单还把它们从清理里豁免掉。⇒ 换 cache_id 清得掉 DNS 缓存，清不掉 FakeIP 的地址表与计数器。
///
/// 留这段是因为「换个 cache_id 就能把 FakeIP 投毒/错配洗掉」是个很自然、且**试了也不会报错**的
/// 猜想 —— 它只是静默无效。FakeIP 错配的实际缓解在 `builder::dns` 的 `FAKEIP_REWRITE_TTL`。
///
/// **已对随包内核亲验**（2026-08-10，此前标注的「未重新验证」可撤）：首次核于 v1.14.0-beta.7
/// （`3001f038`），抬核到 v1.14.0-beta.12（`426c5faf`）后复核 `experimental/cachefile/cache.go`
/// 在两版之间**逐字未变**，故下述行号与判据在随包核上继续成立。
/// `experimental/cachefile/cache.go:215` 的启动清理判据逐字是
/// `if !(common.Contains(bucketNameList, bucketName) || strings.HasPrefix(bucketName, fakeipBucketPrefix))`
/// —— fakeip 前缀被**显式豁免**，且 `bucketNameList` 本就不含它们。三个桶是顶层桶，不经 `cacheID`。
const CACHE_ID: &str = "polaris-dns-v2";

/// 上游 `ProxyManager.withRaceOff`：race off 时 clone config，强制 dnsConfig.resolveNodeDomainsAhead=false。
///
/// race server 未就绪（off/起失败/snapshot/preflight/诊断）→ getNodeResolverTag/buildDnsConfig 一致走
/// 单上游、不引用 dns-node-race（防 FATAL，快照零变化）。clone 后仅清该字段，其余 dns_config 原样保留。
fn with_race_off(config: &UserConfig) -> UserConfig {
    let mut cfg = config.clone();
    if let Some(dns) = cfg.dns_config.as_mut() {
        dns.resolve_node_domains_ahead = Some(false);
    } else {
        // 上游 `{ ...config.dnsConfig, resolveNodeDomainsAhead: false }`：dnsConfig 为 undefined 时
        // spread 空对象 → 结果 { resolveNodeDomainsAhead: false }。Rust 侧 Some(默认 + 该字段)。
        cfg.dns_config = Some(crate::user_config::dns_config::DnsConfig {
            resolve_node_domains_ahead: Some(false),
            ..Default::default()
        });
    }
    cfg
}

/// 选中节点不可用（naive 缺 libcronet）的用户可见原因。上游 `naiveUnavailableReason`。
///
/// 移植为纯函数：Polaris 读 resourceManager.getCronetLibStatus()（'copy-failed' 分支）+ process.platform。
/// 此处注入 has_cronet（恒 false 触发）+ platform + 可选 copy_failed 标志（对拍/生产可省）。
fn naive_unavailable_reason(server_name: &str, copy_failed: bool, platform: &str) -> String {
    if copy_failed {
        return format!(
            "选中的节点「{server_name}」是 NaiveProxy：libcronet 核心库已内置，但拷贝到核心目录失败\
             （可能是权限/磁盘空间/杀软占用）。请重启应用重试或检查目录权限；如仍失败，请改用其它协议的节点。"
        );
    }
    if platform.eq_ignore_ascii_case("darwin") {
        return format!(
            "选中的节点「{server_name}」是 NaiveProxy，但当前 macOS 核心未内置 cronet\
             （暂无官方预编译库）。请选择其它协议的节点。"
        );
    }
    format!(
        "选中的节点「{server_name}」是 NaiveProxy，但未找到 libcronet 核心库。请选择其它协议的节点。"
    )
}

/// 上游 `isNodeUsable`：naive 需要 libcronet，缺库不可用（其余恒可用）。
fn is_node_usable(
    server: &crate::user_config::server_config::ServerConfig,
    has_cronet: bool,
) -> bool {
    // !(naive && !has_cronet) ⟺ naive != Naive || has_cronet。
    server.protocol != Protocol::Naive || has_cronet
}

/// dashboard 的显式 HTTP client：`detour` 取 `route.final`。
///
/// **为什么是 `route.final`（而非 direct，也非顶层 `http_clients` + `route.default_http_client`）**：
///
/// 1. **等价性**：被替换掉的隐式回落在核里是 `DefaultOutbound = true`（`box.go` 的
///    `httpClientManager.Initialize` 回落工厂）→ `NewDefaultOutboundDetour(outboundManager)`
///    → `outboundManager.Default()`，而默认出站正是 `route.final` 指的那个 tag。写死 `direct`
///    会把 dashboard 的下载腿从「走代理」改成「走直连」——在 `dashboard_serve_dir=None` 的
///    联网兜底路径上，这是**用户可见的行为回退**（GitHub 直连在墙内拉不动）。
/// 2. **不选顶层 `http_clients` + `route.default_http_client`**：那是上游文档给的通用写法，但
///    (a) 它给**每一份**配置都加顶层键，而本仓 37 例金样无一含 `services`，等于为零消费者付
///    全量夹具 delta；(b) `httpclient.Manager.Start()` 在 `defaultTag != ""` 时**急切**解析默认
///    transport——dashboard 关着（本仓默认 `singboxDashboard=false`）的用户本来一个 transport
///    都不建，加了反而白建一个。作用域收到真实消费点上，两笔成本都不付。
///
/// `route.final` 缺省（理论上不可达：`build_route_config` 恒 `Some`）时回落 `"direct"` ——
/// 空 `detour` 会让核把 `http_client` 判成 `IsEmpty()` 而**重新落回隐式默认**，等于本改动失效，
/// 故必须给非空 tag 而不是留空。
fn dashboard_http_client(singbox: &SingBoxConfig) -> HttpClient {
    let detour = singbox
        .route
        .as_ref()
        .and_then(|r| r.final_outbound.clone())
        .unwrap_or_else(|| "direct".to_string());
    HttpClient { detour }
}

/// generateSingBoxConfig 依赖注入：Polaris 所有 `this.*` 实例态。
///
/// 对拍：FS 路径注入固定假路径（如 "/fake/cache.db"），回调为 no-op。生产由 ProxyManager 等价层填真值。
#[derive(Debug, Clone)]
pub struct GenerateConfigDeps {
    /// process.platform（neighbor match / mesh system 门控 / log output 谓词）。
    pub platform: String,
    /// 编译目标 arch（outbound tls_spoof 门控）。
    pub arch: String,
    /// 本地 race DNS server 端口（>0 = race 就绪；0 = race off → withRaceOff）。
    pub race_server_port: u16,
    pub probe_direct_port: Option<u16>,
    pub probe_proxy_port: Option<u16>,
    pub update_in_port: Option<u16>,
    /// §15 主核测速探测池：K 个 probe-selector-k 端口。空 = 不注入池。
    pub probe_pool_ports: Vec<u16>,
    pub lan_resolver_for_dns: Option<String>,
    /// race 就绪时的自定义上游 IP（route 直连放行防 TUN 回环）。Polaris L3556 raceServerPort>0 才传。
    pub race_upstream_ips: Vec<String>,
    /// 上面那些上游**实际在用的端口**（`polaris-dns-race` 的 `ResolvedUpstreams::direct_ports` 下发）。
    /// 与 [`race_upstream_ips`](Self::race_upstream_ips) 同源同命：同样只在 `race_server_port > 0` 时透传，
    /// 缺省空 = race off。route 侧只消费不复算（见 `RouteConfigDeps::race_upstream_ports`）。
    pub race_upstream_ports: Vec<u16>,
    /// libcronet 库已内置（naive 协议可用性）。has_cronet=false 时选中 naive 节点 → Err。
    pub has_cronet: bool,
    /// libcronet 拷贝失败（copy-failed 状态）：naive_unavailable_reason 选对应文案。
    pub cronet_copy_failed: bool,
    /// sing-box 1.14 management API 可用（coreVersionAtLeast 1.14）。false → 不注入 services。
    pub has_management_api: bool,
    /// privacyProvider()：隐私模式（buildLogConfig 抬 ≥warn）。
    pub privacy_mode: bool,
    /// buildLogConfig 输入：日志级别（Polaris config.logLevel || 'info'）。UserConfig 增量子集未含此字段。
    pub log_level: crate::user_config::LogLevel,
    /// buildLogConfig 输入：禁用日志写盘（Polaris config.disableLogFile）。UserConfig 增量子集未含此字段。
    pub disable_log_file: bool,
    /// sing-box 核心 dashboard 服务目录解析结果（resolveDashboardServeDir）。None = 不注入 dashboard.path。
    /// Polaris resolveDashboardServeDir 返回 override 或 bundled 或 null（两者皆无）。
    pub dashboard_serve_dir: Option<String>,
    /// tailscale management API 监听端口（services[0].listen_port）。
    pub tailscale_api_port: u16,
    /// experimental.cache_file.path（Polaris getCachePath = <userData>/cache.db）。
    pub cache_path: String,
    /// TUN 模式 sing-box 日志文件路径（buildLogConfig output）。None = TUN 时 output 留空。
    pub log_file_path: Option<String>,
    // ── FS/路径注入（子 builder 共用，对拍固定假路径）──
    pub runtime_rules_dir: String,
    pub rule_resources_path: String,
    pub custom_rules_dir: String,
    pub tailscale_state_dir_prefix: String,
    /// FS 存在性 + SRS 魔数检查（dns/route geo rule_set fail-closed）。对拍 fixture 注入固定 true/false。
    pub is_valid_srs_fn: fn(&str) -> bool,
    /// 本机所有非回环接口 CIDR（buildInbounds own_lan_cidrs）。Polaris getOwnLanCidrs。
    pub own_lan_cidrs: Vec<String>,
    /// 日志回调（子 builder log）。Polaris (level, message) => this.logToManager —— 此处降级为单参 message。
    pub log: fn(LogLevel, &str),
    /// customRuleFiles 降级回调（route onDegraded）。Polaris () => this.customRuleFilesDegraded = true。
    pub on_degraded: fn(),
}

/// sing-box 配置生成。上游 `ProxyManager.generateSingBoxConfig`（L3470-3637）1:1 移植。
///
/// 纯函数 + 依赖注入：所有实例态经 `deps` 传入。返回完整 `SingBoxConfig` 或用户可见错误（选中节点
/// 不存在 / naive 缺 libcronet / detour 死引用命中选中节点）。
///
/// **与 Polaris 的有意差异**：
/// - cache_id = "polaris-dns-v2"（Polaris "polaris-dns-v2"）—— §D.2 品牌归一化。
/// - 不写回 `this.currentIdToTagMap` / `this.pendingRuleSelectors` / `this.currentRuleTargetMap`：
///   这些是 ProxyManager 实例态（热切换用），config-engine 是纯库无实例态。调用方（Polaris ProxyManager
///   等价层）从返回的 SingBoxConfig + idToTagMap 自行维护。本函数仅返回最终 config。
/// - `currentRuleTargetMap` 回填逻辑（L3618-3628，过滤 liveSelectorTags）属热切换态，不在此移植。
pub fn generate_sing_box_config(
    config: &UserConfig,
    resolved_ips: &BTreeMap<String, String>,
    deps: &GenerateConfigDeps,
) -> Result<SingBoxConfig, String> {
    generate_sing_box_config_with_report(config, resolved_ips, deps).map(|o| o.config)
}

/// 启动 gate 剔除的单个非法节点（前端 `InvalidNodeInfo` 的 1:1 镜像）。
///
/// 仅会话内存语义：每次起核重判，换核自动复活（对齐前端契约注释）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvalidNode {
    /// 节点 id（`ServerConfig.id`）。
    pub id: String,
    /// 该节点在生成集合里本该占的 outbound tag（**剔除前**的 tag，供日志/tooltip 对人可读）。
    pub tag: String,
    /// 成因判别符，取值见 [`INVALID_REASON_DETOUR_CASCADE`] 等同级 const。
    pub reason: String,
}

/// [`generate_sing_box_config_with_report`] 的产物：最终 config + 本次 gate 的剔除报告。
#[derive(Debug, Clone)]
pub struct GenerateOutcome {
    /// 生成的 sing-box config（与 [`generate_sing_box_config`] 返回值逐字节相同）。
    pub config: SingBoxConfig,
    /// 本次生成被 gate 剔除的节点。**空 Vec 是有意义的值**（= 本次无非法节点 → 渲染端据此清陈旧标灰），
    /// 调用方不得因「空就跳过」而吞掉它。
    pub invalid_nodes: Vec<InvalidNode>,
    /// 因本地 `.srs` 缺失/损坏被 fail-closed 剪枝的 rule_set tag（见 [`crate::builder::route::RouteConfigOutcome`]）。
    ///
    /// **空 = 规则集完整**。非空 ⟺ 本次生成真的丢了分流规则 → 运行时层据此发用户可见信号
    /// （`RULE_RESOURCES_MISSING`）并收紧出口自证白名单。资源齐全时恒空 ⇒ 不产生噪音。
    pub pruned_rule_set_tags: Vec<String>,
}

/// [`generate_sing_box_config`] + 剔除报告。
///
/// **为什么另开一个入口而非改原签名**：`generate_sing_box_config` 有 202/202 golden 对拍
/// （`tests/golden_config_snapshot.rs`）+ 多处调用方，改返回类型会把纯粹的「多返回一个副产物」
/// 变成全仓签名 churn。原函数保留为本函数的薄 wrapper（同一条代码路径，绝无第二份生成逻辑
/// → 不存在「两个入口算出不同 config」的分叉面）。
pub fn generate_sing_box_config_with_report(
    config: &UserConfig,
    resolved_ips: &BTreeMap<String, String>,
    deps: &GenerateConfigDeps,
) -> Result<GenerateOutcome, String> {
    // ── 1. withRaceOff（L3473）──────────────────────────────────────────────────
    // race server 就绪（raceServerPort>0）才走 race 解析；否则强制 race off。
    let cfg = if deps.race_server_port > 0 {
        config.clone()
    } else {
        with_race_off(config)
    };

    // ── 2. selectedServer 校验（L3475-3487）─────────────────────────────────────
    // direct / block 哨兵都不是节点 id：其出口由 proxy-selector 的 default 直接接到内置出站
    // （`direct` / `block`），没有节点承载 ⇒ 必须豁免存在性与可用性校验，否则 0 节点或纯哨兵
    // 配置会在这里报 "Selected server not found" 而**根本起不了核**。
    let is_sentinel = is_sentinel_selection(config.selected_server_id.as_deref());
    let selected_server = if is_sentinel {
        None
    } else {
        config
            .servers
            .iter()
            .find(|s| Some(s.id.as_str()) == config.selected_server_id.as_deref())
    };
    if !is_sentinel {
        let server = selected_server.ok_or_else(|| "Selected server not found".to_string())?;
        if !is_node_usable(server, deps.has_cronet) {
            return Err(naive_unavailable_reason(
                &server.name,
                deps.cronet_copy_failed,
                &deps.platform,
            ));
        }
    }

    // ── 3. idToTagMap（L3495）───────────────────────────────────────────────────
    // 预生成 ID→Tag 唯一映射（节点名作 tag，拓扑/日志友好）。dns/route/outbounds 共用单一真值。
    // ServerLike 包装：build_id_to_tag_map 接受 trait，ServerConfig 需薄包装（与 outbounds.rs 一致）。
    struct SrvLike<'a>(&'a crate::user_config::server_config::ServerConfig);
    impl<'a> ServerLike for SrvLike<'a> {
        fn id(&self) -> &str {
            &self.0.id
        }
        fn name(&self) -> &str {
            &self.0.name
        }
    }
    let wrappers: Vec<SrvLike> = config.servers.iter().map(SrvLike).collect();
    let id_to_tag_map = build_id_to_tag_map(&wrappers);

    // ── 4. buildOutbounds（L3501-3515，先行）────────────────────────────────────
    // 产 pendingEndpoints / pendingRuleSelectors，供 route/dns 消费。
    let system_interface_available = matches!(config.proxy_mode_type, ProxyModeType::Tun)
        && mesh_system_supported_on_platform(&deps.platform);
    let mut outbounds_deps = OutboundsDeps {
        platform: deps.platform.clone(),
        arch: deps.arch.clone(),
        gate_invalid_nodes: std::collections::BTreeMap::new(),
        system_interface_available,
        probe_pool_ports: deps.probe_pool_ports.clone(),
        tailscale_state_dir_prefix: deps.tailscale_state_dir_prefix.clone(),
        has_cronet_lib: deps.has_cronet,
        log: deps.log,
    };
    let outbounds_result = build_outbounds(&cfg, &mut outbounds_deps)?;
    let pending_endpoints = outbounds_result.pending_endpoints.clone();

    // ── 5. buildLogConfig（L3518）───────────────────────────────────────────────
    // proto Platform::parse 兼容 "darwin"/"win32"，未知串 → Other；log builder 视 Other 同 Linux
    // （TUN 下三平台 + Other 均写文件），故与原 `_ => Linux` 行为等价。
    let log_platform = Platform::parse(deps.platform.as_str());
    let log_input = LogConfigInput {
        log_level: deps.log_level,
        disable_log_file: deps.disable_log_file,
        proxy_mode_type: config.proxy_mode_type,
    };
    let log = build_log_config(
        &log_input,
        &LogBuildDeps {
            privacy_mode: deps.privacy_mode,
            platform: log_platform,
            log_file_path: deps.log_file_path.as_deref(),
        },
    );

    // ── 6. buildDnsConfig（L3519-3533）──────────────────────────────────────────
    // selectedServerTag 恒 'proxy-selector'（Polaris L3521 硬编码）。
    let dns_deps = DnsConfigDeps {
        lan_resolver_for_dns: deps.lan_resolver_for_dns.clone(),
        pending_endpoints: pending_endpoints.clone(),
        log: deps.log,
        // DNS 侧的 detour 必须跟随 route 侧**同一条**出口回退（2026-08-11 修）。
        //
        // 此前这里是字面量 `"proxy-selector"`。选中「关外网的组网节点」时 route 侧整体回退 direct
        // （`route.rs` 的 D4/D7 块），而 selector 的 `default` 仍是那个组网节点 ⇒ `dns-remote`
        // 的 DoH 查询被送进它，再被 WireGuard 的 cryptokey routing 按 `allowed_ips` 丢掉。
        //
        // 实测取证（本地探针，非推断）：该状态下生成的配置里
        //   route.final = "direct"
        //   proxy-selector = { default: "wg1", outbounds: ["wg1","direct"] }
        //   dns-remote     = { server: "dns.google", detour: "proxy-selector" }
        // 而 wg1 的 allowed_ips 是 10.0.0.0/24 —— dns.google 不在该段 ⇒ **每一次远程解析必然超时**。
        //
        // 改为跟随回退**严格不劣于现状**：现状是 100% 丢包；改后只在「DoH 端点本身被直连屏蔽」时失败。
        // 不改 selector 的 `default`（那会让用户在面板/Clash API 里看到自己选的节点被换掉），
        // 只改 DNS 这一处的 detour —— 与 route 侧的回退同源同时机。
        selected_server_tag: if crate::builder::route::mesh_selected_exit_falls_back_to_direct(
            config,
        ) {
            "direct".to_string()
        } else {
            "proxy-selector".to_string()
        },
        race_server_port: deps.race_server_port,
        probe_pool_ports: deps.probe_pool_ports.clone(),
        probe_proxy_port: deps.probe_proxy_port,
        platform: deps.platform.clone(),
        custom_rules_dir: deps.custom_rules_dir.clone(),
        runtime_rules_dir: deps.runtime_rules_dir.clone(),
        is_valid_srs_fn: deps.is_valid_srs_fn,
        // ext JSON source 存在性走 existsSync 等价（生产真 FS）。见 RouteConfigDeps 处同款说明。
        exists_fn: crate::builder::custom_rule_files::ext_rule_file_exists,
    };
    let dns = build_dns_config(&cfg, &id_to_tag_map, &dns_deps);

    // ── 7. buildInbounds（L3534-3541）───────────────────────────────────────────
    let inbounds_deps = InboundsDeps {
        probe_direct_port: deps.probe_direct_port,
        probe_proxy_port: deps.probe_proxy_port,
        update_in_port: deps.update_in_port,
        probe_pool_ports: deps.probe_pool_ports.clone(),
        platform: deps.platform.clone(),
        own_lan_cidrs: deps.own_lan_cidrs.clone(),
        log: deps.log,
    };
    let inbounds = build_inbounds(config, Some(resolved_ips), &inbounds_deps);

    // ── 8. buildRouteConfig（L3543-3557）────────────────────────────────────────
    // race 就绪时把上游 IP **与端口**一起传 route 直连放行（防 TUN 回环，两轴缺一规则匹配不上）；
    // 未就绪两轴恒 []（`race_server_port == 0` ⟺ race off ⟺ 端口集回 `[53,443]` 基线，金样不动）。
    let (race_upstream, race_upstream_ports) = if deps.race_server_port > 0 {
        (
            deps.race_upstream_ips.clone(),
            deps.race_upstream_ports.clone(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let route_deps = RouteConfigDeps {
        probe_direct_port: deps.probe_direct_port,
        probe_proxy_port: deps.probe_proxy_port,
        update_in_port: deps.update_in_port,
        probe_pool_ports: deps.probe_pool_ports.clone(),
        lan_resolver_for_dns: deps.lan_resolver_for_dns.clone(),
        pending_endpoints: &pending_endpoints,
        log: deps.log,
        on_degraded: deps.on_degraded,
        race_upstream_ips: race_upstream,
        race_upstream_ports,
        runtime_rules_dir: deps.runtime_rules_dir.clone(),
        rule_resources_path: deps.rule_resources_path.clone(),
        custom_rules_dir: deps.custom_rules_dir.clone(),
        arch: deps.arch.clone(),
        platform: deps.platform.clone(),
        is_valid_srs_fn: deps.is_valid_srs_fn,
    };
    let route_outcome = build_route_config_with_report(config, &id_to_tag_map, &route_deps);
    let route = route_outcome.route;

    // ── 9. 装配顶层 SingBoxConfig + experimental.cache_file（L3517-3571）────────
    let mut singbox = SingBoxConfig {
        log,
        dns: Some(dns),
        inbounds,
        outbounds: outbounds_result.outbounds.clone(),
        endpoints: None,
        route: Some(route),
        experimental: Some(Experimental {
            cache_file: Some(CacheFile {
                enabled: true,
                path: deps.cache_path.clone(),
                cache_id: Some(CACHE_ID.to_string()),
                store_fakeip: Some(true),
                store_dns: Some(true),
            }),
        }),
        services: None,
    };

    // ── 10. endpoints 注入顶层（L3575-3577）─────────────────────────────────────
    if !pending_endpoints.is_empty() {
        singbox.endpoints = Some(pending_endpoints.clone());
    }

    // ── 11. services 注入（L3581-3600，has_management_api 门控）──────────────────
    if deps.has_management_api {
        let secret = config.clash_api_secret.clone();
        let mut api_service = ApiService {
            type_field: "api".to_string(),
            listen: "127.0.0.1".to_string(),
            listen_port: deps.tailscale_api_port,
            secret,
            dashboard: None,
        };
        // dashboard opt-in：仅 config.singboxDashboard==true 时注入。
        if config.singbox_dashboard == Some(true) {
            let http_client = Some(dashboard_http_client(&singbox));
            api_service.dashboard = Some(match &deps.dashboard_serve_dir {
                Some(dir) => ApiDashboard {
                    enabled: true,
                    path: Some(dir.clone()),
                    http_client,
                },
                None => ApiDashboard {
                    enabled: true,
                    path: None,
                    http_client,
                },
            });
        }
        singbox.services = Some(vec![api_service]);
    }

    // ── 12. fixRouteDeadReferences（L3605）──────────────────────────────────────
    // route 规则指向「已被跳过/不存在的出站」→ sing-box "outbound not found" 启动失败。改写为 proxy-selector。
    if let Some(route) = singbox.route.as_mut() {
        fix_route_dead_references(&singbox.outbounds, &pending_endpoints, &mut route.rules);
    }

    // ── 13. 调试日志（L3631-3634）───────────────────────────────────────────────
    let rule_set_count = singbox
        .route
        .as_ref()
        .and_then(|r| r.rule_set.as_ref())
        .map(|v| v.len())
        .unwrap_or(0);
    (deps.log)(
        LogLevel::Info,
        &format!(
            "配置已生成: inbounds={}, outbounds={}, rule_set={}",
            singbox.inbounds.len(),
            singbox.outbounds.len(),
            rule_set_count
        ),
    );

    // ── gate 剔除报告（EVENT_PROXY_INVALID_NODES 的真值源）────────────────────────
    // `build_outbounds` 把被剔的 id 记进 `outbounds_deps.gate_invalid_nodes`（`&mut` 出参）；此前它
    // 随 deps 一起在函数末尾被丢弃 → 「哪些节点被剔」这个真值**产生了却没人拿得到**，渲染端的
    // `invalidNodes` store 因此恒空。此处把它连同 tag/reason 一并交回调用方。
    //
    // tag 取自 `id_to_tag_map`（步骤 3 生成，**剔除前**的全量映射）：`prune_detour_dead_references`
    // 只从它自己的局部 `id_to_tag` 里 remove，不动这份 → 被剔节点的 tag 在此仍查得到，正是 tooltip 要的。
    //
    // reason **随剔除点记录**（`BTreeMap<id, token>`），不在此处写死：成因已不止 detour 级联一种
    // （control_url 非法是第二种），写死会让 tooltip 报出与真实成因无关的那一个。
    let invalid_nodes: Vec<InvalidNode> = outbounds_deps
        .gate_invalid_nodes
        .iter()
        .map(|(id, reason)| InvalidNode {
            id: id.clone(),
            tag: id_to_tag_map.get(id).cloned().unwrap_or_default(),
            reason: (*reason).to_string(),
        })
        .collect();

    Ok(GenerateOutcome {
        config: singbox,
        invalid_nodes,
        pruned_rule_set_tags: route_outcome.pruned_rule_set_tags,
    })
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::builder::outbounds::INVALID_REASON_DETOUR_CASCADE;
    use crate::user_config::proxy_mode::{ProxyMode, ProxyModeType};
    use crate::user_config::server_config::{Protocol, SecurityMode, ServerConfig};

    /// 构造最小 GenerateConfigDeps（Linux、race off、无 probe、FS 全 false）。
    fn deps_default() -> GenerateConfigDeps {
        GenerateConfigDeps {
            platform: "linux".into(),
            arch: "x64".into(),
            race_server_port: 0,
            probe_direct_port: None,
            probe_proxy_port: None,
            update_in_port: None,
            probe_pool_ports: vec![],
            lan_resolver_for_dns: None,
            race_upstream_ips: vec![],
            race_upstream_ports: vec![],
            has_cronet: true,
            cronet_copy_failed: false,
            has_management_api: false,
            privacy_mode: false,
            log_level: crate::user_config::LogLevel::Info,
            disable_log_file: false,
            dashboard_serve_dir: None,
            tailscale_api_port: 15490,
            cache_path: "/fake/cache.db".into(),
            log_file_path: Some("/fake/singbox.log".into()),
            runtime_rules_dir: "/fake/runtime-rules".into(),
            rule_resources_path: "/fake/rule-resources".into(),
            custom_rules_dir: "/fake/custom-rules".into(),
            tailscale_state_dir_prefix: "/fake/ts".into(),
            is_valid_srs_fn: |_| false,
            own_lan_cidrs: vec![],
            log: |_, _| {},
            on_degraded: || {},
        }
    }

    /// 最小 UserConfig：smart + systemProxy + 单 vless 节点。
    fn base_config() -> UserConfig {
        UserConfig {
            servers: vec![ServerConfig {
                id: "s1".into(),
                name: "HK".into(),
                protocol: Protocol::Vless,
                address: "hk.example.com".into(),
                port: 443,
                uuid: Some("u".into()),
                security: Some(SecurityMode::Tls),
                ..Default::default()
            }],
            selected_server_id: Some("s1".into()),
            proxy_mode: ProxyMode::Smart,
            proxy_mode_type: ProxyModeType::SystemProxy,
            ..Default::default()
        }
    }

    /// 造一份「合法 vless（选中） + 一个 Tailscale 节点」的配置，TS 的 `control_url` 由入参给定。
    fn config_with_ts_control_url(control_url: &str) -> UserConfig {
        use crate::user_config::server_config::TailscaleSettings;
        let mut cfg = base_config();
        cfg.servers.push(ServerConfig {
            id: "ts1".into(),
            name: "我的 headscale".into(),
            protocol: Protocol::Tailscale,
            tailscale_settings: Some(TailscaleSettings {
                control_url: Some(control_url.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        cfg
    }

    /// **拦截必须发生在「下发到核」之前** —— 这条钉的是发射面，不是谓词。
    ///
    /// `control_url.rs` 的单测只证明「谓词判得对」；就算谓词全绿，只要 `outbounds.rs` 里那段 gate
    /// 没接上，坏 endpoint 照样会进 `config.endpoints` 被写进磁盘配置、交给内核去 panic。
    /// 故这里断言的是**最终产物**：endpoints 里不得出现该节点，且 `invalid_nodes` 要带对成因。
    ///
    /// **变异实测（真跑过）**：把 `outbounds.rs` 里 Tailscale 分支那段 `if let Some(reject) = …
    /// { … continue; }` 整段删掉 ⇒ 本测转红（endpoints 里冒出 `control_url` 为 IP 的 endpoint，
    /// 且 `invalid_nodes` 为空）。
    #[test]
    fn ip_literal_control_url_never_reaches_generated_endpoints() {
        for (url, want_token) in [
            ("http://192.168.1.10:8080", "control-url-ip"),
            ("https://127.0.0.1:39824", "control-url-ip"),
            ("http://[fd7a:115c:a1e0::1]:8080", "control-url-ip"),
            ("hs.example.com", "control-url-scheme"),
            ("http://:8080", "control-url-invalid"),
        ] {
            let cfg = config_with_ts_control_url(url);
            let out = generate_sing_box_config_with_report(&cfg, &BTreeMap::new(), &deps_default())
                .expect("坏 TS 节点只该被剔除，不该让整份配置生成失败");

            // ① 发射面：endpoints 里不得有任何带这个 control_url 的条目。
            let eps = out.config.endpoints.clone().unwrap_or_default();
            assert!(
                !eps.iter().any(|e| e.control_url.is_some()),
                "control_url={url} 的 endpoint 竟被下发到内核配置里（gate 没接上）"
            );
            assert!(
                !eps.iter().any(|e| e.tag.contains("headscale")),
                "被剔节点的 endpoint 仍出现在配置里: {url}"
            );

            // ② 报告面：成因要带对 token（这是用户 tooltip 的真值源）。
            let n = out
                .invalid_nodes
                .iter()
                .find(|n| n.id == "ts1")
                .unwrap_or_else(|| panic!("control_url={url} 未被记进 invalid_nodes"));
            assert_eq!(n.reason, want_token, "成因 token 不对: {url}");

            // ③ 其余节点不受牵连（只剔坏节点，不 FATAL 整份配置）。
            assert!(
                out.config.outbounds.iter().any(|o| o.tag.contains("HK")),
                "合法节点被无辜牵连: {url}"
            );
        }
    }

    /// **阴性对照**：域名形式的 `control_url` 必须照常下发。
    ///
    /// 没有这条，把 gate 写成「所有 Tailscale 节点一律剔除」也能让上面那条全绿 —— 那样的门
    /// 只是把 panic 换成了「Tailscale 永远用不了」。`localhost` 明确在**放行**侧（实测 check 通过）。
    ///
    /// **变异实测（真跑过）**：把 `tailscale_control_url_reject` 改成恒 `Some(IpLiteral)`
    /// ⇒ 本测转红（endpoints 为空），而上面那条阳性测试仍全绿。
    #[test]
    fn domain_control_url_still_reaches_generated_endpoints() {
        for url in [
            "https://hs.example.com",
            "http://localhost:8080",
            "https://controlplane.tailscale.com",
        ] {
            let cfg = config_with_ts_control_url(url);
            let out = generate_sing_box_config_with_report(&cfg, &BTreeMap::new(), &deps_default())
                .expect("合法 TS 节点应能生成");
            let eps = out.config.endpoints.clone().unwrap_or_default();
            assert!(
                eps.iter().any(|e| e.control_url.as_deref() == Some(url)),
                "合法 control_url={url} 没被下发（阴性对照失败 = gate 误伤）"
            );
            assert!(
                !out.invalid_nodes.iter().any(|n| n.id == "ts1"),
                "合法 control_url={url} 被误记进 invalid_nodes"
            );
        }
    }

    /// 造「detour 级联剔除」场景：naive 节点缺 cronet 被丢 → 链到它的 ss 节点 detour 死引用被剔。
    /// 返回 (config, deps)。selected 是独立的合法 vless（保证生成成功、剔除的是非选中节点）。
    fn config_with_cascade_invalid() -> (UserConfig, GenerateConfigDeps) {
        use crate::user_config::protocol_settings::{NaiveSettings, ShadowsocksSettings};
        let selected = ServerConfig {
            id: "sel".into(),
            name: "SEL".into(),
            protocol: Protocol::Vless,
            address: "sel.example.com".into(),
            port: 443,
            uuid: Some("u".into()),
            security: Some(SecurityMode::Tls),
            ..Default::default()
        };
        // naive 节点：deps.has_cronet=false 时在 build_outbounds 里被 `continue` 丢弃（不进 outbounds）。
        let naive = ServerConfig {
            id: "nv".into(),
            name: "NAIVE".into(),
            protocol: Protocol::Naive,
            address: "nv.example.com".into(),
            port: 443,
            naive_settings: Some(NaiveSettings { use_http3: None }),
            ..Default::default()
        };
        // ss 节点：detour 指向被丢的 naive → detour 死引用 → 被 prune 剔除 + 记进 gate_invalid_nodes。
        let chained = ServerConfig {
            id: "ch".into(),
            name: "CHAINED".into(),
            protocol: Protocol::Shadowsocks,
            address: "ch.example.com".into(),
            port: 8388,
            detour: Some("nv".into()),
            shadowsocks_settings: Some(ShadowsocksSettings {
                method: "aes-256-gcm".into(),
                password: "p".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = UserConfig {
            servers: vec![selected, naive, chained],
            selected_server_id: Some("sel".into()),
            proxy_mode: ProxyMode::Smart,
            proxy_mode_type: ProxyModeType::SystemProxy,
            ..Default::default()
        };
        let mut deps = deps_default();
        deps.has_cronet = false; // 逼 naive 节点被丢
        (config, deps)
    }

    #[test]
    fn report_surfaces_cascade_invalid_node_with_id_tag_reason() {
        let (config, deps) = config_with_cascade_invalid();
        let outcome =
            generate_sing_box_config_with_report(&config, &BTreeMap::new(), &deps).unwrap();
        // 「ch」经 nv 的死 detour 被剔 → 必须现身报告，且带 id / 非空 tag / detour-cascade reason。
        // 这一断言锁死的是 generate.rs 末尾「把 gate_invalid_nodes 映射成 InvalidNode」那段接线：
        // 删掉那段 → invalid_nodes 恒空 → 本测转红（变异验证见 report_empty_when_no_invalid_nodes 反面）。
        assert_eq!(outcome.invalid_nodes.len(), 1, "恰一个级联剔除节点");
        let n = &outcome.invalid_nodes[0];
        assert_eq!(n.id, "ch", "记录被剔的引用方 id");
        assert!(!n.tag.is_empty(), "tag 取自剔除前的 id_to_tag_map，非空");
        assert_eq!(n.reason, INVALID_REASON_DETOUR_CASCADE, "成因=detour 级联");
        // 被丢的 naive 本身不进报告（它是 continue 跳过，非 prune 剔除，无 gate 记录）——
        // 报告只含「因死引用被主动剔」的节点，语义与前端 tooltip 一致。
        assert!(
            !outcome.invalid_nodes.iter().any(|x| x.id == "nv"),
            "naive 被 continue 丢弃，不计入 gate 剔除报告"
        );
        // config 本身仍生成成功（选中节点合法）。
        assert!(!outcome.config.outbounds.is_empty());
    }

    #[test]
    fn report_empty_when_no_invalid_nodes() {
        // 全合法配置 → 报告空 Vec（**有意义的空**：渲染端据此清陈旧标灰）。
        let outcome =
            generate_sing_box_config_with_report(&base_config(), &BTreeMap::new(), &deps_default())
                .unwrap();
        assert!(
            outcome.invalid_nodes.is_empty(),
            "无非法节点时报告为空（非 None，是空 Vec）"
        );
    }

    #[test]
    fn wrapper_and_report_produce_identical_config() {
        // `generate_sing_box_config` 是 `_with_report` 的薄 wrapper → 二者 config 必须逐字节同源
        // （证「多返回一个副产物」没有派生出第二条生成路径）。
        let (config, deps) = config_with_cascade_invalid();
        let via_wrapper = generate_sing_box_config(&config, &BTreeMap::new(), &deps).unwrap();
        let via_report = generate_sing_box_config_with_report(&config, &BTreeMap::new(), &deps)
            .unwrap()
            .config;
        assert_eq!(
            serde_json::to_value(&via_wrapper).unwrap(),
            serde_json::to_value(&via_report).unwrap(),
            "wrapper 与 report 入口生成的 config 必须完全一致"
        );
    }

    #[test]
    fn generate_returns_full_config_with_required_sections() {
        let cfg = base_config();
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default()).unwrap();
        // log/dns/inbounds/outbounds/route/experimental 恒存在。
        assert!(!result.inbounds.is_empty(), "inbounds non-empty");
        assert!(!result.outbounds.is_empty(), "outbounds non-empty");
        assert!(result.dns.is_some(), "dns present");
        assert!(result.route.is_some(), "route present");
        assert!(result.experimental.is_some(), "experimental present");
        // services 未注入（has_management_api=false）。
        assert!(result.services.is_none());
    }

    #[test]
    fn cache_file_has_polaris_brand_id_and_store_flags() {
        let result =
            generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps_default()).unwrap();
        let cache = result
            .experimental
            .as_ref()
            .unwrap()
            .cache_file
            .as_ref()
            .unwrap();
        assert!(cache.enabled);
        assert_eq!(cache.path, "/fake/cache.db");
        assert_eq!(cache.cache_id.as_deref(), Some("polaris-dns-v2"));
        assert_eq!(cache.store_fakeip, Some(true));
        assert_eq!(cache.store_dns, Some(true));
    }

    #[test]
    fn direct_selection_skips_server_validation() {
        // __direct__ 哨兵 → 不校验 selectedServer（即使 servers 空也不报错）。
        let mut cfg = base_config();
        cfg.selected_server_id = Some("__direct__".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default());
        assert!(result.is_ok(), "直连哨兵不报错");
    }

    /// __block__ 哨兵同样豁免 selectedServer 校验 —— 漏了这条，选阻断后**根本起不了核**
    /// （报 "Selected server not found"），而 UI 那侧只会显示一个点了没反应的按钮。
    ///
    /// 变异锁：把 `is_sentinel_selection` 换回 `is_direct_selection` → 转红。
    #[test]
    fn block_selection_skips_server_validation() {
        let mut cfg = base_config();
        cfg.selected_server_id = Some("__block__".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default());
        assert!(result.is_ok(), "阻断哨兵不报错: {:?}", result.err());
    }

    /// 零节点 + 阻断哨兵也必须能生成（阻断出口不需要任何节点承载）。
    #[test]
    fn block_selection_generates_with_zero_servers() {
        let mut cfg = base_config();
        cfg.servers = vec![];
        cfg.selected_server_id = Some("__block__".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default());
        assert!(result.is_ok(), "零节点阻断不报错: {:?}", result.err());
    }

    #[test]
    fn missing_selected_server_returns_error() {
        let mut cfg = base_config();
        cfg.selected_server_id = Some("nonexistent".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default());
        assert_eq!(result.unwrap_err(), "Selected server not found");
    }

    #[test]
    fn naive_without_cronet_returns_unavailable_error() {
        // 选中 naive 节点 + has_cronet=false → Err（不静默切节点）。
        let mut cfg = base_config();
        cfg.servers[0].protocol = Protocol::Naive;
        cfg.servers[0].name = "NaiveNode".into();
        let mut deps = deps_default();
        deps.has_cronet = false;
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps);
        let err = result.unwrap_err();
        assert!(err.contains("NaiveNode"), "错误含节点名");
        assert!(err.contains("libcronet"), "错误含 libcronet 原因");
    }

    #[test]
    fn naive_without_cronet_copy_failed_branch() {
        let mut cfg = base_config();
        cfg.servers[0].protocol = Protocol::Naive;
        cfg.servers[0].name = "N".into();
        let mut deps = deps_default();
        deps.has_cronet = false;
        deps.cronet_copy_failed = true;
        let err = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap_err();
        assert!(err.contains("拷贝到核心目录失败"), "copy-failed 文案");
    }

    #[test]
    fn naive_without_cronet_darwin_branch() {
        let mut cfg = base_config();
        cfg.servers[0].protocol = Protocol::Naive;
        cfg.servers[0].name = "N".into();
        let mut deps = deps_default();
        deps.has_cronet = false;
        deps.platform = "darwin".into();
        let err = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap_err();
        assert!(err.contains("macOS"), "darwin 文案");
    }

    #[test]
    fn naive_with_cronet_is_usable() {
        // naive + has_cronet=true → 可用（isNodeUsable 通过）。
        let mut cfg = base_config();
        cfg.servers[0].protocol = Protocol::Naive;
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default());
        assert!(result.is_ok(), "naive + cronet 可用");
    }

    #[test]
    fn race_off_forces_resolve_node_domains_ahead_false() {
        // raceServerPort=0 → withRaceOff → dnsConfig.resolveNodeDomainsAhead=false。
        // 不应有 dns-node-race server（race off）。
        let result =
            generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps_default()).unwrap();
        let dns = result.dns.as_ref().unwrap();
        assert!(
            dns.servers.iter().all(|s| s.tag != "dns-node-race"),
            "race off 不生成 dns-node-race"
        );
    }

    #[test]
    fn race_on_emits_race_server() {
        // raceServerPort>0 → dns-node-race server。
        let mut deps = deps_default();
        deps.race_server_port = 5353;
        let result = generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps).unwrap();
        let dns = result.dns.as_ref().unwrap();
        assert!(
            dns.servers.iter().any(|s| s.tag == "dns-node-race"),
            "race on 生成 dns-node-race"
        );
    }

    /// 取 DNS 直连放行规则的端口集（`ip_cidr` 含引导 DNS + route→direct 的那条）。
    fn dns_direct_ports(result: &SingBoxConfig) -> Vec<u32> {
        let route = result.route.as_ref().expect("route 必在");
        let rule = route
            .rules
            .iter()
            .find(|r| {
                r.outbound.as_deref() == Some("direct")
                    && r.ip_cidr
                        .as_ref()
                        .is_some_and(|c| c.contains(&"223.5.5.5/32".to_string()))
            })
            .expect("DNS 直连放行规则必存在");
        match rule.port.as_ref().expect("该规则必带端口集") {
            crate::singbox::OneOrMany::One(p) => vec![*p],
            crate::singbox::OneOrMany::Many(v) => v.clone(),
        }
    }

    /// 【不变式：`race_server_port == 0` 时上游两轴一律不透传】
    ///
    /// race off 与「起 sidecar 失败」在生成侧是同一种状态（port=0）。此时哪怕 deps 里还残留着上一轮的
    /// 上游 IP/端口（运行期状态翻转与 config 生成之间有窗口），也不得放行 —— 放行一个没人在监听的
    /// 端口是白开口子，且会让金样输出随残留值漂移。
    ///
    /// **变异锁**：把 `deps.race_server_port > 0` 的门去掉（两轴无条件透传）→ 本测的
    /// 「`8443` 不得出现」转红；只对 IP 轴留门、端口轴直传 → 同样转红。
    #[test]
    fn race_off_drops_both_upstream_axes() {
        let mut deps = deps_default();
        deps.race_server_port = 0; // race off
        deps.race_upstream_ips = vec!["9.9.9.9".to_string()]; // 残留值
        deps.race_upstream_ports = vec![8443];
        let result = generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps).unwrap();

        assert_eq!(
            dns_direct_ports(&result),
            vec![53, 443],
            "race off → 端口集回基线"
        );
        let route = result.route.as_ref().unwrap();
        assert!(
            !route.rules.iter().any(|r| r
                .ip_cidr
                .as_ref()
                .is_some_and(|c| c.contains(&"9.9.9.9/32".to_string()))),
            "race off → 残留的上游 IP 同样不得放行"
        );
    }

    /// 【不变式：race on 时上游两轴**一起**透传到 route】
    ///
    /// **变异锁**：把 `race_upstream_ports` 那路改成恒 `Vec::new()`（只传 IP）→ `8443` 断言转红。
    #[test]
    fn race_on_forwards_both_upstream_axes_to_route() {
        let mut deps = deps_default();
        deps.race_server_port = 5353;
        deps.race_upstream_ips = vec!["9.9.9.9".to_string()];
        deps.race_upstream_ports = vec![8443];
        let result = generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps).unwrap();

        let ports = dns_direct_ports(&result);
        assert!(
            ports.contains(&8443),
            "上游端口须随 IP 一起进直连放行（两轴缺一规则匹配不上），实得 {ports:?}"
        );
        let route = result.route.as_ref().unwrap();
        assert!(
            route.rules.iter().any(|r| r
                .ip_cidr
                .as_ref()
                .is_some_and(|c| c.contains(&"9.9.9.9/32".to_string()))),
            "上游 IP 须进直连放行"
        );
    }

    #[test]
    fn endpoints_injected_when_present() {
        // WireGuard 节点 → pendingEndpoints 非空 → 顶层 endpoints 注入。
        let mut cfg = base_config();
        cfg.servers[0] = ServerConfig {
            id: "wg1".into(),
            name: "WARP".into(),
            protocol: Protocol::Wireguard,
            address: "engage.cloudflareclient.com".into(),
            port: 2408,
            wireguard_settings: Some(crate::user_config::server_config::WireGuardSettings {
                private_key: Some("priv".into()),
                local_address: vec!["172.16.0.2/32".into()],
                peer_public_key: Some("pub".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        cfg.selected_server_id = Some("wg1".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default()).unwrap();
        assert!(
            result.endpoints.is_some(),
            "WireGuard 节点 → endpoints 注入顶层"
        );
        assert!(!result.endpoints.as_ref().unwrap().is_empty());
    }

    // ══════════════════════════════════════════════════════════════════════════
    // endpoint 前置代理（detour）—— 对 上游的**有意偏离**（上游 三个组网表单与
    // `SingBoxEndpoint` 类型都没有 detour）。语义实测与「WG 需 UDP 转发」见
    // `singbox/endpoint.rs` 的 `Endpoint::detour`。
    //
    // 三条门都断言**序列化后的 JSON**（不是 struct 字段），因为这一整条接线的失效模式就是
    // 「struct 上有值、serde 把它丢了」——`Endpoint` 结构体本轮之前根本没有这个字段，
    // WarpDialog 那个 select 写进 `server.detour` 后在生成侧被静默丢弃，是个装饰开关。
    // ══════════════════════════════════════════════════════════════════════════

    /// 三种 endpoint（普通 WG / WARP / Tailscale）＋一个代理节点，前三者 detour 全指向后者。
    /// selected 另取一个独立 vless，保证生成成功、被测的三个都是非选中节点。
    fn config_with_three_endpoint_detours() -> UserConfig {
        use crate::user_config::server_config::{TailscaleSettings, WireGuardSettings};
        let selected = ServerConfig {
            id: "sel".into(),
            name: "SEL".into(),
            protocol: Protocol::Vless,
            address: "sel.example.com".into(),
            port: 443,
            uuid: Some("u".into()),
            security: Some(SecurityMode::Tls),
            ..Default::default()
        };
        // 前置代理本体（detour 目标）。
        let front = ServerConfig {
            id: "front".into(),
            name: "FRONT".into(),
            protocol: Protocol::Vless,
            address: "front.example.com".into(),
            port: 443,
            uuid: Some("u".into()),
            security: Some(SecurityMode::Tls),
            ..Default::default()
        };
        let wg = ServerConfig {
            id: "wg1".into(),
            name: "WG".into(),
            protocol: Protocol::Wireguard,
            address: "wg.example.com".into(),
            port: 51820,
            detour: Some("front".into()),
            wireguard_settings: Some(WireGuardSettings {
                private_key: Some("priv".into()),
                peer_public_key: Some("pub".into()),
                local_address: vec!["10.0.0.2/32".into()],
                allow_internet: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        // WARP：判据是端点域名（`domain/warp.ts` / `crate::warp::is_warp_server`），不是名字。
        // 它同走 `build_wireguard_endpoint`，但会额外过 `downgrade_mesh` 那段后处理——
        // 这条门顺带钉住「后处理不得把 detour 抹掉」。
        let warp = ServerConfig {
            id: "warp1".into(),
            name: "WARP".into(),
            protocol: Protocol::Wireguard,
            address: "engage.cloudflareclient.com".into(),
            port: 2408,
            detour: Some("front".into()),
            wireguard_settings: Some(WireGuardSettings {
                private_key: Some("priv".into()),
                peer_public_key: Some("pub".into()),
                local_address: vec!["172.16.0.2/32".into()],
                allow_internet: Some(true),
                reverse_mesh: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ts = ServerConfig {
            id: "ts1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            detour: Some("front".into()),
            tailscale_settings: Some(TailscaleSettings {
                exit_node: Some("exit-peer".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        UserConfig {
            servers: vec![selected, front, wg, warp, ts],
            selected_server_id: Some("sel".into()),
            proxy_mode: ProxyMode::Smart,
            proxy_mode_type: ProxyModeType::SystemProxy,
            ..Default::default()
        }
    }

    /// 【门 ①】三种 endpoint 的 detour 都真的落进生成的 JSON，且值 = 前置代理的 **outbound tag**。
    ///
    /// 期望 tag 不写死字面量，而是从产物里按 `server` 地址反查那个 outbound 的 tag ——
    /// 手拼 `"proxy-front"` 会在 `build_id_to_tag_map` 改命名规则时静默变成一条永假的断言。
    #[test]
    fn endpoint_detour_lands_in_generated_json() {
        let cfg = config_with_three_endpoint_detours();
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default()).unwrap();
        let json = serde_json::to_value(&result).unwrap();

        let front_tag = json["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["server"] == "front.example.com")
            .and_then(|o| o["tag"].as_str())
            .expect("前置代理 outbound 必须在产物里")
            .to_string();

        let eps = json["endpoints"]
            .as_array()
            .expect("endpoints 必须注入顶层");
        assert_eq!(eps.len(), 3, "三个 endpoint 全发射，实得 {eps:?}");
        for want_type in ["wireguard", "tailscale"] {
            assert!(
                eps.iter().any(|e| e["type"] == want_type),
                "{want_type} endpoint 必须在产物里"
            );
        }
        for ep in eps {
            assert_eq!(
                ep["detour"].as_str(),
                Some(front_tag.as_str()),
                "endpoint「{}」的 detour 必须序列化进 JSON 且等于前置代理 tag",
                ep["tag"]
            );
        }
    }

    /// 【门 ②】detour 目标是 endpoint 类节点 → 排除（沿用代理 outbound 早就在用的同一条），
    /// 但**引用方本身必须留在产物里**（只丢 detour，不丢节点）。
    ///
    /// 变异对照：删掉 `resolve_detour_tag` 里的 `is_mesh_protocol` 那支 ⇒ WG 的 detour 变成
    /// TS 的 endpoint tag，而 `valid_tags` 只取自 outbounds ⇒ 剪枝把整个 WG endpoint 剔掉 ⇒
    /// 「WG 仍在」这条断言转红。
    #[test]
    fn endpoint_detour_target_endpoint_excluded() {
        use crate::user_config::server_config::{TailscaleSettings, WireGuardSettings};
        let mut cfg = base_config();
        cfg.servers.push(ServerConfig {
            id: "ts1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            tailscale_settings: Some(TailscaleSettings::default()),
            ..Default::default()
        });
        cfg.servers.push(ServerConfig {
            id: "wg1".into(),
            name: "WG".into(),
            protocol: Protocol::Wireguard,
            address: "wg.example.com".into(),
            port: 51820,
            detour: Some("ts1".into()), // ← 目标是 endpoint
            wireguard_settings: Some(WireGuardSettings {
                private_key: Some("priv".into()),
                peer_public_key: Some("pub".into()),
                local_address: vec!["10.0.0.2/32".into()],
                allow_internet: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default()).unwrap();
        let json = serde_json::to_value(&result).unwrap();
        let eps = json["endpoints"]
            .as_array()
            .expect("endpoints 必须注入顶层");
        let wg = eps
            .iter()
            .find(|e| e["type"] == "wireguard")
            .expect("WG endpoint 必须仍在产物里（只丢 detour，不丢节点）");
        assert!(
            wg.get("detour").is_none(),
            "detour 目标是 endpoint ⇒ 该键根本不得出现，实得 {wg:?}"
        );
    }

    /// 【门 ②b】detour 目标是 **openconnect / openvpn-client** → 同样排除。
    ///
    /// 它们落 `endpoints[]`、tag 不在 `outbounds[]` 里，指向它们的 detour 与指向 WG/TS 是同一类
    /// 悬空引用。此前判据用的是只认 WG/TS 的 `is_mesh_protocol`，这两个协议漏在外面 ——
    /// 后果不是「多一个没用的选项」，而是**引用方整个节点被剪掉并上报 invalid**（用户侧：节点没了）。
    ///
    /// 变异对照：把 `resolve_detour_tag` 的判据改回 `is_mesh_protocol` ⇒ 本条断言转红。
    #[test]
    fn detour_target_openconnect_excluded() {
        use crate::user_config::protocol_settings::OpenconnectSettings;
        let mut cfg = base_config();
        cfg.servers.push(ServerConfig {
            id: "oc1".into(),
            name: "OC".into(),
            protocol: Protocol::Openconnect,
            openconnect_settings: Some(OpenconnectSettings {
                server: Some("vpn.example.com:443".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        cfg.servers.push(ServerConfig {
            id: "v1".into(),
            name: "V".into(),
            protocol: Protocol::Vless,
            address: "v.example.com".into(),
            port: 443,
            uuid: Some("11111111-1111-1111-1111-111111111111".into()),
            detour: Some("oc1".into()), // ← 目标是 endpoint 腿
            ..Default::default()
        });
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default()).unwrap();
        let json = serde_json::to_value(&result).unwrap();
        // 按 tag 定位而不是按 type：`base_config()` 本身就带一个 vless 节点，按 type 找会命中它 ——
        // 那个节点从来就没有 detour，断言恒绿、变异不红（落地时先踩了这一下）。
        let vless = json["outbounds"]
            .as_array()
            .expect("outbounds")
            .iter()
            .find(|o| o["tag"] == "V")
            .expect("引用方必须留在产物里 —— 只丢 detour，不丢节点");
        assert!(
            vless.get("detour").is_none(),
            "detour 目标落在 endpoints[] ⇒ 该键根本不得出现，实得 {vless:?}"
        );
    }

    /// 用户为 openconnect / openvpn-client 声明的内网段，必须真的变成 force-route 规则。
    ///
    /// 这是「组网资格由节点决定」那条判据的**产出侧**验证：不声明 ⇒ 没有任何规则指向它（它只是个
    /// 普通出口）；声明了 ⇒ 该段被路由到它自己的 tag，与一个填了 `allowedIPs` 的 WG 节点无分别。
    ///
    /// 变异对照：删掉 `endpoint_forced_route_cidrs` 里的 openconnect/openvpn 那支 ⇒ 第二段断言转红。
    #[test]
    fn declared_mesh_routes_become_force_route_rules() {
        use crate::user_config::protocol_settings::OpenconnectSettings;
        let mk = |routes: Vec<String>| {
            let mut cfg = base_config();
            cfg.servers.push(ServerConfig {
                id: "oc1".into(),
                name: "OC".into(),
                protocol: Protocol::Openconnect,
                mesh_routes: routes,
                openconnect_settings: Some(OpenconnectSettings {
                    server: Some("vpn.example.com:443".into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let r = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps_default()).unwrap();
            serde_json::to_value(&r).unwrap()
        };

        let oc_tag = |json: &serde_json::Value| -> String {
            json["endpoints"]
                .as_array()
                .expect("endpoints")
                .iter()
                .find(|e| e["type"] == "openconnect")
                .expect("openconnect 必须落 endpoints[]")["tag"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let routed_cidrs = |json: &serde_json::Value, tag: &str| -> Vec<String> {
            json["route"]["rules"]
                .as_array()
                .map(|rs| {
                    rs.iter()
                        .filter(|r| r["outbound"].as_str() == Some(tag))
                        .filter_map(|r| r["ip_cidr"].as_array())
                        .flatten()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        // 不声明 → 只是个普通出口，没有任何段被强制路由过去。
        let bare = mk(vec![]);
        let bare_tag = oc_tag(&bare);
        assert!(
            routed_cidrs(&bare, &bare_tag).is_empty(),
            "没声明内网段的 openconnect 不该有 force-route 规则"
        );

        // 声明 → 该段进 force-route。0/0 被剥掉（全隧道是另一件事，由出网开关表达）。
        let declared = mk(vec!["10.10.0.0/16".into(), "0.0.0.0/0".into()]);
        let declared_tag = oc_tag(&declared);
        assert_eq!(
            routed_cidrs(&declared, &declared_tag),
            vec!["10.10.0.0/16".to_string()],
            "声明的内网段必须被路由到该节点自己的 tag，且 catch-all 不混进来"
        );
    }

    /// 【门 ③】endpoint 的悬空 detour（目标节点在生成集合里不存在）→ 整个 endpoint 被剪掉，
    /// 不进产物、不留在 selector 成员里，并作为「detour 级联剔除」上报给渲染端。
    ///
    /// 场景复用既有的 naive-缺-cronet 造死引用手法（`config_with_cascade_invalid` 同款）：
    /// naive 节点在发射循环里被 `continue` 丢弃，而 `id_to_tag` 仍有它的条目 ⇒
    /// WG 的 detour 解析成一个**没有对应 outbound** 的 tag。
    ///
    /// 变异对照：删掉 `prune_detour_dead_references` 的 endpoint 腿 ⇒ 悬空 detour 原样进产物
    /// （真核起核即 FATAL，本地测不到那一步）⇒ 前两条断言转红。
    #[test]
    fn endpoint_dangling_detour_pruned_from_output() {
        use crate::user_config::protocol_settings::NaiveSettings;
        use crate::user_config::server_config::WireGuardSettings;
        let mut cfg = base_config();
        cfg.servers.push(ServerConfig {
            id: "nv".into(),
            name: "NAIVE".into(),
            protocol: Protocol::Naive,
            address: "nv.example.com".into(),
            port: 443,
            naive_settings: Some(NaiveSettings { use_http3: None }),
            ..Default::default()
        });
        cfg.servers.push(ServerConfig {
            id: "wg1".into(),
            name: "WG".into(),
            protocol: Protocol::Wireguard,
            address: "wg.example.com".into(),
            port: 51820,
            detour: Some("nv".into()),
            wireguard_settings: Some(WireGuardSettings {
                private_key: Some("priv".into()),
                peer_public_key: Some("pub".into()),
                local_address: vec!["10.0.0.2/32".into()],
                allow_internet: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut deps = deps_default();
        deps.has_cronet = false; // 逼 naive 被丢 → WG 的 detour 成悬空引用
        let outcome = generate_sing_box_config_with_report(&cfg, &BTreeMap::new(), &deps).unwrap();
        let json = serde_json::to_value(&outcome.config).unwrap();

        let eps = json["endpoints"].as_array().cloned().unwrap_or_default();
        assert!(
            !eps.iter().any(|e| e["type"] == "wireguard"),
            "悬空 detour 的 WG endpoint 必须被剪掉，实得 {eps:?}"
        );
        // selector 成员表里也不得留它的 tag（否则 selector 引用不存在的 tag，同样 FATAL）。
        let dangling_in_selector = json["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["type"] == "selector")
            .filter_map(|o| o["outbounds"].as_array())
            .flatten()
            .any(|m| m.as_str() == Some("WG"));
        assert!(
            !dangling_in_selector,
            "被剪掉的 endpoint tag 不得留在任何 selector 成员表里"
        );
        // 上报给渲染端（标灰 + tooltip 归因），与 outbound 腿同一个 reason token。
        assert!(
            outcome
                .invalid_nodes
                .iter()
                .any(|n| n.id == "wg1" && n.reason == INVALID_REASON_DETOUR_CASCADE),
            "被剪的 endpoint 必须进 invalid_nodes 报告，实得 {:?}",
            outcome.invalid_nodes
        );
    }

    #[test]
    fn services_not_injected_without_management_api() {
        let result =
            generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps_default()).unwrap();
        assert!(
            result.services.is_none(),
            "无 management API 不注入 services"
        );
    }

    #[test]
    fn services_injected_with_management_api() {
        let mut deps = deps_default();
        deps.has_management_api = true;
        let result = generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps).unwrap();
        let services = result.services.as_ref().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].type_field, "api");
        assert_eq!(services[0].listen, "127.0.0.1");
        assert_eq!(services[0].listen_port, 15490);
        assert!(services[0].dashboard.is_none(), "singboxDashboard 未开");
    }

    #[test]
    fn services_include_clash_api_secret() {
        let mut cfg = base_config();
        cfg.clash_api_secret = Some("secret123".into());
        let mut deps = deps_default();
        deps.has_management_api = true;
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap();
        let svc = &result.services.as_ref().unwrap()[0];
        assert_eq!(svc.secret.as_deref(), Some("secret123"));
    }

    #[test]
    fn dashboard_injected_when_opted_in() {
        let mut cfg = base_config();
        cfg.singbox_dashboard = Some(true);
        let mut deps = deps_default();
        deps.has_management_api = true;
        deps.dashboard_serve_dir = Some("/fake/dashboard".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap();
        let dash = result.services.as_ref().unwrap()[0]
            .dashboard
            .as_ref()
            .unwrap();
        assert!(dash.enabled);
        assert_eq!(dash.path.as_deref(), Some("/fake/dashboard"));
        // 显式 HTTP client：detour 必须逐字等于 route.final（= 核的默认出站），
        // 否则就是把「隐式回落走默认出站」悄悄改成了走别的出站。
        let final_tag = result.route.as_ref().unwrap().final_outbound.clone();
        assert_eq!(
            dash.http_client.as_ref().map(|h| h.detour.clone()),
            final_tag,
            "dashboard.http_client.detour 必须 = route.final"
        );
    }

    #[test]
    fn dashboard_enabled_without_serve_dir_omits_path() {
        // singboxDashboard=true 但 serve_dir=None → dashboard.enabled=true、path 省略（核联网兜底）。
        let mut cfg = base_config();
        cfg.singbox_dashboard = Some(true);
        let mut deps = deps_default();
        deps.has_management_api = true;
        deps.dashboard_serve_dir = None;
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap();
        let dash = result.services.as_ref().unwrap()[0]
            .dashboard
            .as_ref()
            .unwrap();
        assert!(dash.enabled);
        assert!(dash.path.is_none(), "无 serve_dir 时 path 省略");
        // 这条路径恰恰是**唯一真的会用到**该 transport 的路径（无本地 dashboard → 核联网拉取），
        // 故 http_client 在此不可缺省。
        let final_tag = result.route.as_ref().unwrap().final_outbound.clone();
        assert_eq!(
            dash.http_client.as_ref().map(|h| h.detour.clone()),
            final_tag,
            "联网兜底路径上 dashboard.http_client 更不能缺"
        );
    }

    #[test]
    fn dashboard_not_injected_when_off() {
        let mut cfg = base_config();
        cfg.singbox_dashboard = Some(false);
        let mut deps = deps_default();
        deps.has_management_api = true;
        deps.dashboard_serve_dir = Some("/fake/dashboard".into());
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap();
        assert!(
            result.services.as_ref().unwrap()[0].dashboard.is_none(),
            "singboxDashboard=false 不注入 dashboard"
        );
    }

    #[test]
    fn mesh_system_unavailable_on_win32_tun() {
        // win32 + tun → system_interface_available=false（Windows 禁 system）。
        // TS endpoint 仍发射（gVisor 用户态），system_interface 降级（无 FATAL）。
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.servers[0] = ServerConfig {
            id: "ts1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            address: "".into(),
            port: 0,
            tailscale_settings: Some(
                crate::user_config::server_config::TailscaleSettings::default(),
            ),
            ..Default::default()
        };
        cfg.selected_server_id = Some("ts1".into());
        let mut deps = deps_default();
        deps.platform = "win32".into();
        let result = generate_sing_box_config(&cfg, &BTreeMap::new(), &deps).unwrap();
        // endpoints 非空（TS endpoint 发射，win32 不阻断生成）。
        assert!(result.endpoints.is_some(), "win32 TS endpoint 仍发射");
    }

    #[test]
    fn probe_ports_propagate_to_inbounds_and_dns() {
        // probe_direct/proxy/port 注入 → inbounds 含 probe-direct-in/proxy-in。
        let mut deps = deps_default();
        deps.probe_direct_port = Some(100);
        deps.probe_proxy_port = Some(101);
        deps.update_in_port = Some(102);
        let result = generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps).unwrap();
        let tags: Vec<String> = result.inbounds.iter().map(|i| i.tag.clone()).collect();
        assert!(tags.iter().any(|t| t == "probe-direct-in"));
        assert!(tags.iter().any(|t| t == "probe-proxy-in"));
        assert!(tags.iter().any(|t| t == "update-in"));
    }

    #[test]
    fn fix_route_dead_references_applied() {
        // 死引用兜底：即使 route 引用不存在的 outbound，经 fix 后改写 proxy-selector。
        // 此处验证 generate 不 panic 且 route.rules 可迭代（fix 已内联）。
        let result =
            generate_sing_box_config(&base_config(), &BTreeMap::new(), &deps_default()).unwrap();
        let rules_len = result.route.as_ref().map(|r| r.rules.len()).unwrap_or(0);
        // route.rules 非空（至少有 default/dns-hijack 等基础规则），fix 已内联不 panic。
        assert!(rules_len > 0, "route.rules 非空（fix 已应用）");
    }

    #[test]
    fn with_race_off_sets_resolve_ahead_false() {
        let mut cfg = UserConfig::default();
        cfg.dns_config = Some(crate::user_config::dns_config::DnsConfig {
            resolve_node_domains_ahead: Some(true),
            ..Default::default()
        });
        let off = with_race_off(&cfg);
        assert_eq!(
            off.dns_config.as_ref().unwrap().resolve_node_domains_ahead,
            Some(false)
        );
    }

    #[test]
    fn with_race_off_preserves_other_dns_fields() {
        let mut cfg = UserConfig::default();
        cfg.dns_config = Some(crate::user_config::dns_config::DnsConfig {
            resolve_node_domains_ahead: Some(true),
            optimistic_cache: Some(true),
            ..Default::default()
        });
        let off = with_race_off(&cfg);
        // optimistic_cache 原样保留。
        assert_eq!(
            off.dns_config.as_ref().unwrap().optimistic_cache,
            Some(true)
        );
    }

    #[test]
    fn mesh_system_supported_excludes_win32() {
        assert!(!mesh_system_supported_on_platform("win32"));
        assert!(mesh_system_supported_on_platform("darwin"));
        assert!(mesh_system_supported_on_platform("linux"));
        assert!(!mesh_system_supported_on_platform("WIN32")); // 大小写不敏感
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 组合面：生成方（本文件产 selector）× 消费方（hotswitch 规划 PUT 目标）
    //
    // §K7.1 的教训是「A 有门、B 有门、组合面无门」：outbounds 生成 selector 有测试、
    // hotswitch 规划 PUT 也有测试，但**没有任何测试断言二者说的是同一个 tag**。
    // 一旦漂移：PUT 打到不存在的 selector → 核返 NotFound → executor 判 Failed →
    // **静默退回去抖重启** → 用户看到「切换成功」，实际是断流重启，热切换永久失效且无人报错。
    // 下面两条就是那扇缺失的门。
    // ══════════════════════════════════════════════════════════════════════════

    /// 生成产物里**必须真的存在** `PROXY_SELECTOR_TAG` 这个 selector 出站。
    /// 它正是 `plan_hot_switch` 下发 `SelectOutbound` 的目标 —— 不存在即热切换全链路失效。
    #[test]
    fn generated_config_contains_the_selector_that_hotswitch_puts_to() {
        use crate::user_config::dns_constants::PROXY_SELECTOR_TAG;
        let config = base_config();
        let out = generate_sing_box_config(&config, &BTreeMap::new(), &deps_default()).unwrap();
        let sel = out
            .outbounds
            .iter()
            .find(|o| o.tag == PROXY_SELECTOR_TAG)
            .unwrap_or_else(|| {
                panic!(
                    "生成产物里找不到 tag={PROXY_SELECTOR_TAG} 的出站 —— 热切换 PUT 必然 NotFound。\
                     实有出站：{:?}",
                    out.outbounds.iter().map(|o| &o.tag).collect::<Vec<_>>()
                )
            });
        assert_eq!(
            sel.type_field, "selector",
            "{PROXY_SELECTOR_TAG} 必须是 selector 类型，否则 SelectOutbound 无从切换"
        );
    }

    /// `plan_hot_switch` 算出的 `selector_tag`，必须逐条命中生成产物里真实存在的 selector。
    ///
    /// 这条直接对拍「PUT 目标」与「核里实际有什么」——即便将来有人把某处 tag 改回内联字面量，
    /// 只要两侧不一致，这条立刻转红。
    #[test]
    fn hotswitch_plan_put_targets_all_exist_as_selectors_in_generated_config() {
        use crate::builder::hotswitch::{plan_hot_switch, HotSwitchDeps};
        use crate::user_config::dns_constants::PROXY_SELECTOR_TAG;

        // old：选中 node-a；new：切到 node-b（纯值变更 → 走全局热切腿）。
        let mut old = base_config();
        old.servers.push(ServerConfig {
            id: "node-b".into(),
            name: "Node B".into(),
            protocol: Protocol::Shadowsocks,
            address: "2.2.2.2".into(),
            port: 8388,
            ..Default::default()
        });
        old.selected_server_id = Some(old.servers[0].id.clone());
        let mut new = old.clone();
        new.selected_server_id = Some("node-b".into());

        // idToTagMap 与生成侧同源（build_id_to_tag_map）——生产路径也是这么喂的。
        struct S<'a>(&'a ServerConfig);
        impl ServerLike for S<'_> {
            fn id(&self) -> &str {
                &self.0.id
            }
            fn name(&self) -> &str {
                &self.0.name
            }
        }
        let wrappers: Vec<S> = old.servers.iter().map(S).collect();
        let deps = HotSwitchDeps {
            current_id_to_tag_map: Some(build_id_to_tag_map(&wrappers)),
            platform: "linux".into(),
            ..Default::default()
        };

        let plan = plan_hot_switch(&old, &new, &deps);
        assert!(
            !plan.puts.is_empty(),
            "切节点应产出至少一条 PUT（前提失败则本测试失去意义）"
        );

        let out = generate_sing_box_config(&old, &BTreeMap::new(), &deps_default()).unwrap();
        let selectors: Vec<&str> = out
            .outbounds
            .iter()
            .filter(|o| o.type_field == "selector")
            .map(|o| o.tag.as_str())
            .collect();
        for p in &plan.puts {
            assert!(
                selectors.contains(&p.selector_tag.as_str()),
                "PUT 目标 selector `{}` 在生成产物里不存在 → 核会返 NotFound → 静默退回重启。\
                 实有 selector：{selectors:?}",
                p.selector_tag
            );
            // 成员也必须真在该 selector 里，否则 SelectOutbound 同样 NotFound。
            let sel = out
                .outbounds
                .iter()
                .find(|o| o.tag == p.selector_tag)
                .unwrap();
            let members = sel.outbounds.clone().unwrap_or_default();
            assert!(
                members.contains(&p.member_tag),
                "PUT 成员 `{}` 不在 selector `{}` 的成员表里（实有：{members:?}）",
                p.member_tag,
                p.selector_tag
            );
        }
        assert!(
            selectors.contains(&PROXY_SELECTOR_TAG),
            "全局热切腿的目标必须是 {PROXY_SELECTOR_TAG}"
        );
    }

    #[test]
    fn inbound_exclude_warn_survives_full_assembly() {
        // 变异锁：锁死本函数「── 7. buildInbounds ──」块里的 `log: deps.log` 透传。
        // inbounds.rs 自身的单测直调 build_inbounds，测不出 generate.rs 这一行接线——
        // 如果有人把它删掉（编译期即报错，因 InboundsDeps.log 非 Option 无默认值）或换成
        // `log: |_, _| {}` 这种「看似接了、实为 no-op」的静默逃逸（编译能过，行为悄悄失聪），
        // 只有走这条真实装配路径（generate_sing_box_config_with_report）才抓得住。
        thread_local! {
            static SINK: std::cell::RefCell<Vec<String>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        fn capture(_lvl: LogLevel, msg: &str) {
            SINK.with(|s| s.borrow_mut().push(msg.to_string()));
        }

        let mut config = base_config();
        config.proxy_mode_type = ProxyModeType::Tun;
        config.tun_config = Some(crate::user_config::tun_config::TunModeConfig {
            inbound_exclude_cidrs: Some(vec!["not-a-cidr".into()]),
            ..Default::default()
        });

        let mut deps = deps_default();
        deps.platform = "darwin".into();
        deps.log = capture;

        let result = generate_sing_box_config_with_report(&config, &BTreeMap::new(), &deps);
        assert!(result.is_ok(), "生成应成功: {:?}", result.err());

        let warns = SINK.with(|s| s.borrow_mut().drain(..).collect::<Vec<String>>());
        assert!(
            warns.iter().any(|m| m.contains("非法/过宽网段")),
            "InboundsDeps.log 透传断裂：完整装配路径下未见「连入来源排除」非法段告警。实际: {warns:?}"
        );
    }
}
