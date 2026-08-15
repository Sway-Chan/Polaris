import type {
  NaiveSettings,
  Hysteria2Settings,
  HysteriaSettings,
  TorSettings,
  OpenconnectSettings,
  OpenvpnClientSettings,
  SnellSettings,
  TuicSettings,
  WireGuardSettings,
  TailscaleSettings,
  CustomSettings,
  AnyTlsSettings,
  MultiplexSettings,
  ShadowsocksSettings,
  SshSettings,
  ShadowTlsSettings,
  TlsSettings,
  RealitySettings,
  WebSocketSettings,
  GrpcSettings,
  HttpSettings,
} from './types/protocol-settings';

import type { Rule, CustomRuleSet, RuleResource, AppRule, CustomAppPreset } from './types/rules';
import type { SubscriptionProxyPolicy } from '../domain/subscription-proxy';

// ============================================================================
// 协议设置子类型（re-export，保持所有现有 import 路径不变）
// ============================================================================

export type {
  Hysteria2Network,
  TlsSettings,
  RealitySettings,
  WebSocketSettings,
  GrpcSettings,
  HttpSettings,
  Hysteria2ObfsSettings,
  Hysteria2Settings,
  SnellVersion,
  SnellSettings,
  MultiplexSettings,
  TuicSettings,
  NaiveSettings,
  ShadowsocksSettings,
  AnyTlsSettings,
  SshSettings,
  ShadowTlsSettings,
  WireGuardSettings,
  TailscaleSettings,
  CustomSettings,
} from './types/protocol-settings';

// ============================================================================
// 路由规则子类型（re-export）
// ============================================================================

export type {
  RuleAction,
  RuleType,
  RuleCondition,
  Rule,
  SystemProcessInfo,
  CustomRuleSet,
  RuleResourceFormat,
  RuleResourceCategory,
  RuleResource,
  RuleResourceCatalogItem,
  RuleResourceListItem,
  RuleResourceRef,
  RuleResourceDeleteResult,
  RuleResourceDownloadItem,
  RuleResourceDownloadResult,
  RuleResourceProgress,
  RuleResourceCatalogResult,
  AppRule,
  CustomAppPreset,
} from './types/rules';

// ============================================================================
// 运行时状态子类型（re-export）
// ============================================================================

export type {
  ProxyStatus,
  ProxyErrorEvent,
  InvalidNodeInfo,
  ConnectionEntry,
  ConnectionsSnapshot,
  ClosedConnectionEntry,
  ConnectionsClosedSnapshot,
  ConnectionsClosedUpdate,
  ConnectionsAggregate,
  ConnectionAggHost,
  ConnectionAggOutbound,
  ConnectionAggFlow,
  HelperStatus,
  SystemProxyStatus,
  LogEntry,
  TrafficStats,
  IpInfo,
  IpInfoSnapshot,
  ProxyExitBlock,
  ApiResponse,
  AutoStartStatus,
  PlatformInfo,
} from './types/runtime';

export { ProxyErrorCode, isProxyErrorCode, TOPOLOGY_OTHERS_KEY } from './types/runtime';

// ============================================================================
// 基础类型
// ============================================================================

export type ProxyMode = 'global' | 'smart' | 'direct';
export type ProxyModeType = 'systemProxy' | 'tun' | 'manual';
export type Protocol =
  | 'vless'
  | 'trojan'
  | 'hysteria2'
  | 'shadowsocks'
  | 'anytls'
  | 'tuic'
  | 'vmess'
  | 'naive'
  | 'snell'
  | 'socks'
  | 'http'
  | 'ssh'
  | 'wireguard'
  | 'tailscale'
  // 2026-08-11：随包核支持而此前无表单的两个出站。
  // shadowtls 不在此列且非遗漏 —— 它是 shadowsocks 的插件设置（shadowTlsSettings），
  // 生成侧自动造外层出站并接 detour；它是传输层不是出口。
  | 'hysteria'
  | 'tor'
  | 'openconnect'
  | 'openvpn-client'
  | 'custom';
export type Network = 'tcp' | 'ws' | 'grpc' | 'http' | 'httpupgrade';
export type Security = 'none' | 'tls' | 'reality';
export type LogLevel = 'debug' | 'info' | 'warn' | 'error' | 'fatal';

/**
 * `logs:runtimeLevel` 的返回：核**此刻实际**在用的日志级别。
 *
 * `level` 是 sing-box 侧那套级别名（`panic|fatal|error|warn|info|debug|trace`，七档、序与本仓
 * {@link LogLevel} 相反且多两档），故**不复用** `LogLevel` 类型——把两套枚举混成一个类型，
 * 迟早有人拿它去喂 `config.logLevel`。
 *
 * `level: null` = 读不到，`reason` 说明为什么。**后端绝不在这里回落成某个具体级别**：回落出来的
 * 一定是「我写下的值」，这处自证就退化成它本要揭穿的那句谎。
 */
export interface RuntimeLogLevel {
  /** 核在跑的级别名（小写）；`null` = 读不到。 */
  level: string | null;
  /** `notRunning` = 核没在跑（常态）；`unavailable` = 核在跑但读不到（启动中 / 连不上）；`null` = 读到了。 */
  reason: 'notRunning' | 'unavailable' | null;
}
// 'auto' = 默认档：跟随平台映射（macOS→gvisor / Windows·Linux→system），经 shared/tun-stack#resolveTunStack
// 解析成下发给核的具体栈。system/gvisor/mixed = 显式高级兜底。详见 docs/design/tun-stack-option.md。
export type TunStack = 'auto' | 'system' | 'gvisor' | 'mixed';
// UDP NAT 类型档（RFC 3489 三种锥形）。**字段缺席 = 跟随内核默认**，故这里没有 'auto' 成员：
// 「默认」在这套契约里是 `udpNatType === undefined`，不是一个取值（与 TunStack 的 'auto' 相反——
// 那个档恒会解析成具体栈下发，这个档的语义就是一个键都不发）。
// 一档映射内核两个字段 udp_mapping × udp_filtering，表在 crates/config-engine/.../builder/inbounds.rs
// 的 `udp_nat_behaviors`（SoT 在 Rust，前端只传档名，不复制映射表）。
export type UdpNatType = 'fullCone' | 'restrictedCone' | 'portRestrictedCone';

// ============================================================================
// 订阅配置
// ============================================================================

export interface SubscriptionConfig {
  id: string;
  name: string;
  url: string;
  autoUpdate: boolean;
  lastUpdated?: string;
  createdAt: string;
  // 拉取订阅时的 User-Agent 覆盖（per-sub）。优先级：subscription.userAgent ?? config.subscriptionUserAgent ?? 默认。
  // 默认 `Polaris/<版本>`（纯中性）。订阅对话框「自定义 User-Agent」输入框可设置本字段。
  // 【后端行为差异（已登记）】Rust 侧 `resolve_subscription_ua` 用的是 falsy 语义而非 `??`：
  // 显式 `""`／纯空白**不会**胜出，而是继续回落到 config.subscriptionUserAgent → 默认。
  // 差异只在「显式存了空串」这一格（本对话框把空输入折成 undefined，故正常链路不可达；
  // 导入的配置/手改 json 可造出）。取 falsy 是刻意的：发 `User-Agent: ` 空值会让机场按未知客户端
  // 下发错格式或 0 节点，而清空输入框的意图是「不要 per-sub 覆盖」。
  // 另：subscription_update 改动本字段时，后端会一并清掉该订阅的 etag/lastModified
  //（换 UA 换的是响应变体，旧验证器再 304 就永远拿不到新格式）。
  userAgent?: string;
  // 该订阅是否经代理更新（per-sub；默认 false=直连）。仅全局 subscriptionProxyPolicy='follow' 时生效，
  // 'proxy'/'direct' 时被全局覆盖（忽略本字段）。生效求值见 shared/subscription-proxy#resolveSubscriptionViaProxy。
  updateViaProxy?: boolean;
  // 订阅流量/到期信息（从 Subscription-UserInfo header 解析）
  userInfo?: {
    upload?: number; // 已上传字节
    download?: number; // 已下载字节
    total?: number; // 总流量字节
    expire?: number; // 到期时间（Unix timestamp）
  };
  // §16.3.4 条件 GET 验证器（上次 200 响应的 ETag / Last-Modified）：下次拉取带 If-None-Match / If-Modified-Since，
  // 服务端 304 → 短路 parse/reconcile（零节点扰动、省流省渲染）。缺省=首次/无 validator 源，全量 GET（零回归）。
  etag?: string;
  lastModified?: string;
  // 上次解析含 proxy-providers（Clash provider 型订阅）→ 主正文 304 会掩盖 provider 子资源独立变化 → 不发条件头（保守豁免）。
  hasProviders?: boolean;
}

export interface ServerConfig {
  id: string;
  name: string;
  protocol: Protocol;
  address: string;
  port: number;

  // 代理链（前置代理）ID
  detour?: string;

  /**
   * 用户声明的「经该节点可达的内网段」（CIDR）。**只有 openconnect / openvpn-client 读它** ——
   * 这两个协议的网段由服务端在隧道建立后 push、配置期不可知，故只认用户手填的这份；填了它们才具备
   * 组网能力（`isMeshNode`）。WG 的段在 `wireguardSettings.allowedIPs`，TS 的由协议固定，都不走这里。
   *
   * 放顶层而不放各自的 settings 块：那两个块的键名 = sing-box 键名、整体 flatten 下发，塞个内核不认
   * 的键会硬报错。镜像 Rust `ServerConfig::mesh_routes`。
   */
  meshRoutes?: string[];

  subscriptionId?: string;

  // M1：节点归属的 Clash proxy-provider 名（仅经 proxy-providers 解析的节点有值；内联 proxies / 非 Clash
  // 订阅 / 迁移前存量为 undefined）。用于订阅 partial 失败时按 provider 精确 merge-only——只保留失败 provider
  // 名下的下架节点，成功 provider 的真下架正常删除。
  providerName?: string;

  uuid?: string;
  encryption?: string;
  flow?: string;

  // vless/vmess UDP 封装，默认 xudp，可设 '' 或 'packetaddr' 等
  packetEncoding?: string;

  // Trojan 和 Hysteria2 通用
  password?: string;

  username?: string;
  naiveSettings?: NaiveSettings;

  alterId?: number;
  vmessSecurity?: string;

  hysteria2Settings?: Hysteria2Settings;
  hysteriaSettings?: HysteriaSettings;
  torSettings?: TorSettings;
  openconnectSettings?: OpenconnectSettings;
  openvpnClientSettings?: OpenvpnClientSettings;

  tuicSettings?: TuicSettings;

  // WireGuard 特定（sing-box endpoint）
  wireguardSettings?: WireGuardSettings;

  // Tailscale 特定（sing-box endpoint，账号制 mesh）
  tailscaleSettings?: TailscaleSettings;

  // 自定义协议（raw-JSON 透传，第三方内核用）
  customSettings?: CustomSettings;

  anyTlsSettings?: AnyTlsSettings;

  // Multiplex 多路复用（vless/trojan/vmess/ss；reality+vision 不兼容，生成侧 guard）
  multiplexSettings?: MultiplexSettings;

  shadowsocksSettings?: ShadowsocksSettings;

  // Snell 特定（psk 复用通用 password 字段，同 trojan/hysteria2 惯例）
  snellSettings?: SnellSettings;

  sshSettings?: SshSettings;

  // Shadow-TLS 插件（可附加在任意协议上，常用于 SS2022）
  shadowTlsSettings?: ShadowTlsSettings;

  network?: Network;
  security?: Security;

  tlsSettings?: TlsSettings;

  realitySettings?: RealitySettings;

  wsSettings?: WebSocketSettings;
  grpcSettings?: GrpcSettings;
  httpSettings?: HttpSettings;

  createdAt?: string;
  updatedAt?: string;
}

// ============================================================================
// 本地导入（节点 / 订阅）
// ============================================================================

/** 本地导入识别到的来源格式。 */
export type ImportFormat = 'singbox' | 'xray' | 'clash' | 'links' | 'unknown';

/**
 * 本地导入解析结果（主进程 LOCAL_IMPORT_PARSE 返回，渲染端预览用）。
 * nodes 均为「自建」节点（无 subscriptionId）；subscriptions 仅来自 Clash proxy-providers（不联网拉取）。
 */
export interface ImportParseResult {
  nodes: ServerConfig[];
  subscriptions: { name: string; url: string }[];
  stats: {
    imported: number; // 可入库的节点数（含「不支持但透传为 custom」的，使用时置灰）
    unsupported: number; // 其中「当前内核不支持、已透传为 custom」的数量（imported 的子集）
    skipped: number; // 协议不受支持被跳过的节点数
    failed: number; // 解析失败 / 必填字段不全被跳过的节点数
  };
  warnings: string[];
  format: ImportFormat;
}

export interface TunModeConfig {
  // 缺席 = 自动：按「最终栈 × 平台」派生（domain/tun-mtu.ts defaultMtuFor，与 Rust
  // crates/config-engine/.../tun_stack.rs::default_mtu_for 同口径）。在场 = 用户显式值，逐字下发内核。
  // 存量值由 polaris-store 的 migrate_tun_mtu 一次性抹掉——本项此前从未有 UI 入口，磁盘上的
  // 任何值都是程序写的默认，不承载用户意图。
  mtu?: number;
  stack: TunStack;
  autoRoute: boolean;
  strictRoute: boolean;
  interfaceName?: string;
  inet4Address?: string;
  inet6Address?: string;
  // ── P6 局域网网关（sing-box 1.14 LAN 设备识别簇，仅 Linux 生效，见 shared/neighbor.ts）──
  // TUN 按 MAC 过滤进入 TUN 的设备：'include'=仅 macFilterList 内的设备进 TUN / 'exclude'=排除它们。互斥。
  // 仅 Linux + auto_route + auto_redirect 支持；非法 MAC / 非 Linux 时构建期不发射（防 FATAL）。
  macFilterMode?: 'include' | 'exclude';
  macFilterList?: string[]; // EUI-48 MAC，仅在 macFilterMode 设定时消费
  // local DNS 邻居解析：对这些后缀的单标签短名（如 nas.lan）走局域网邻居解析（主机名→IP）。
  // 每条须以 '.' 开头（构建期归一化）；仅 Linux/macOS 生效。供「局域网设备短名访问」场景。
  neighborDomains?: string[];
  // 「连入来源排除」网段（CIDR，v4/v6）：本机作服务端被 off-subnet 私网连入时（如经 ZeroTier→路由器 DNAT），
  // 声明这些来源段绕过 TUN 捕获（追加进 route_exclude_address），使回包走物理网卡、不被 TUN 用户态栈误劫持。
  // 区别于 bypassLANList（route.rules 直连偏好，不影响是否进 TUN）。生成期减 mesh force-route 段 / fakeip 段；
  // macOS 额外减本机物理 LAN 段（NE 反向路由 guard）。详见 docs/design/polaris-tun-lan-exclusion-scenarios.md。
  inboundExcludeCidrs?: string[];
  // 「NAT 类型」档 → TUN inbound 的 udp_mapping × udp_filtering（sing-box 1.14）。
  // 缺席 = 两个键都不下发 = 跟随内核默认（beta.7 上游默认两项均 endpoint_independent，即全锥）。
  // 影响的是本机 UDP 会话允许哪些远端回包进来 ⇒ 联机 / P2P 打洞成功率。收紧只该在需要缩小 UDP
  // 暴露面时用；缺省即最宽松，游戏场景无需改动。
  udpNatType?: UdpNatType;
}

export interface DnsConfig {
  domesticDns: string; // 国内 DNS，默认 https://doh.pub/dns-query
  foreignDns: string; // 海外 DNS，默认 https://dns.google/dns-query
  enableFakeIp: boolean; // 是否启用 FakeIP（systemProxy / TUN 统一生效，纯看此开关，见 usesFakeIp）
  // FakeIP 开关统一一次性迁移标记：存量旧默认 enableFakeIp:false 多非用户意图，
  // migrateFakeIpToggle 按迁移时刻 proxyModeType 写 effective 值（TUN/manual→true、systemProxy→保留）后置 true。
  // undefined=未迁移（旧配置）；迁移幂等，置 true 后永不再改写，避免覆盖用户后续手动改的值。
  fakeIpToggleMigrated?: boolean;
  // FakeIP-TUN 待纠正快照（三态，见 shared/fakeip-tun-entry.ts + ConfigManager.migrateFakeIpTunPending）：
  // undefined=未评估（仅存量升级瞬间）；true=enableFakeIp:false 判定为 systemProxy 迁移冻结产物，首次进入 TUN
  // 时自动回 true 并消费；false=已消费/已否决/用户已手动表达 FakeIP 意图，永不自动改。
  fakeIpTunAutoEnable?: boolean;
  // ── 节点域名解析上游选择（issue #147 多源 race，取代 #57 单选档位 + 烧 IP）。设计 docs/design/issue147-node-dns-race-resolver.md ──
  // @deprecated 旧单选档位，仅供迁移读取（auto→pool[ali,dnspod] / dnspod→[dnspod] / system→[system]）；
  // 迁移后不再写入，新逻辑读 nodeResolverPool/nodeResolverSingle。旧配置无此字段 → 视为 'auto'。
  nodeDomainResolver?: 'auto' | 'dnspod' | 'system';
  // 节点域名解析容错（issue #147，原「resolve-ahead 前置烧 IP」重定义）：缺省/true=开（默认）。
  // 开 → domain_resolver 指本地 race server（dns-node-race）：多上游 DoH 并发 race（最快非空 wins），多 A 透传给内核 DialSerial 逐个重试。
  // 关 → domain_resolver 按 nodeResolverSingle 指【单上游】（逃生阀，退回单点）。与上游选择正交（设计 §9.1）。旧配置无 → 视为开。
  resolveNodeDomainsAhead?: boolean;
  // race【on】勾选的上游 id 列表（内置 'ali'|'dnspod'|'system' + 自定义 id）。Tier1(ali/dnspod/自定义DoH)抢跑、上限 3；
  // Tier2(system/自定义UDP)兜底不占额度。空 → 回退默认 ['ali','dnspod']。缺省即默认。
  nodeResolverPool?: string[];
  // race【off】单选的上游 id。缺省 'ali'。与 pool 各存各的，切换互不覆盖。
  nodeResolverSingle?: string;
  // 用户自定义上游（强制纯 IP：parseDnsServerSpec.isDomain 为 true 拒绝）。id 稳定供 pool/single 引用。
  nodeResolverCustom?: CustomDnsUpstream[];
  // 旧 nodeDomainResolver → 新多源模型一次性迁移标记（参照 fakeIpToggleMigrated）。undefined=未迁移。
  nodeResolverMigrated?: boolean;
  // TUN 模式强制接管系统 DNS（SystemDnsManager）：缺省/true=开（默认）。on-link 的 LAN/ISP DNS 不进 TUN →
  // hijack-dns 看不到 → 需代理的域名走系统解析得到真实/错族 IP（双栈站 ERR_CONNECTION_CLOSED / 真 v6 泄漏）。
  // 开 → TUN 启动把系统 DNS 改为可路由的 8.8.8.8 使其经 TUN 被 hijack；停止/退出/崩溃还原。
  // 关 → 不接管（dns-local 恢复用原 LAN DNS、内网域名正常，但 TUN 下有上述泄漏风险），供有自定义/内网 DNS 的用户用。
  // 仅 TUN 模式生效；旧配置无此字段 → 视为开。详见 docs/design/dns-ipv6-takeover.md。
  takeoverSystemDns?: boolean;
  // P2b 乐观 DNS 缓存（sing-box 1.14 `dns.optimistic`）：缺省/false=关。开 → 缓存过期后先返回旧值、后台异步刷新，
  // 降低尾延迟（牺牲首条过期响应的新鲜度）。映射到顶层 dns.optimistic:true（仅 true 时下发，关时省略保字节）。
  optimisticCache?: boolean;
  // P2c DNS 查询超时（sing-box 1.14 `dns.timeout`，毫秒）：缺省/未设=用核默认（不下发）。>0 时下发 dns.timeout:"<n>ms"，
  // 治理慢上游不拖整体解析。sanitize 丢弃 ≤0 / 非有限 / 超合理范围（1..60000ms）的值（见 ConfigManager.validateConfig）。
  dnsTimeoutMs?: number;
}

// issue #147：自定义节点解析上游（强制纯 IP，DoH/DoT/UDP）。spec 经 shared/dns.parseDnsServerSpec 校验
// （isDomain=true 拒绝，保证零 bootstrap + route 直连放行确定）。详见 docs/design/issue147-node-dns-race-resolver.md §9.1。
export interface CustomDnsUpstream {
  id: string; // 稳定 id，供 DnsConfig.nodeResolverPool / nodeResolverSingle 引用
  spec: string; // 'https://223.5.5.5/dns-query'(DoH) | 'tls://<IP>:853'(DoT) | '<IP>'(裸 IP=UDP:53)
}

// ============================================================================
// 地区分流（4.1.0）：智能分流的 geo 基线场景
// ============================================================================

/** 地区 ID：选用哪套地区 geo（cn/ir/ru）。 */
export type RegionId = 'cn' | 'ir' | 'ru';
/** 地区分流配置。undefined（存量/新装未改）= 默认中国大陆正向 = 今日智能分流行为（零迁移，见 domain/region-routing）。 */
export interface RegionRoutingConfig {
  enabled: boolean; // 总开关：true=按地区自动分流(本地直连·海外代理)；false=只用自定义规则
  region: RegionId; // 选用哪套地区 geo
  reverse: boolean; // 反向：本地走代理·海外直连（region=cn+reverse=回国）
}

/**
 * §2 待应用差集：节点集相对**运行核起核快照**的差异。均为 serverId 列表；
 * 三个都空 = 运行核与 config 一致（或核未运行）。
 *
 * 后端单一真值 = `ProxyRuntime::pending_changes()`（`src-tauri/src/runtime/proxy.rs`）。
 * pull（`proxy:getPendingChanges`）与 push（`event:proxyPendingChanges`）返回**同一个结构**，
 * 后端无适配层 —— 故此处三字段必须与 Rust 侧逐字段对齐。
 *
 * - `added`   = 磁盘 config 有、起核快照无 → 未入运行核的新节点（徽标「待入池」）。
 * - `modified`= 两侧都有、但**全维**投影不等 → 核里跑的已不是当前配置（徽标「待生效」）。
 * - `removed` = 起核快照有、磁盘 config 无 → 已删但运行核仍持有。
 *
 * `modified` 与测速 dirty 的关系是 **dirty ⊆ modified**，不是相等：二者回答两个问题
 * ——「核里跑的还是不是当前配置」（全维）vs「池里那个出口还能不能代表这个节点」（5 维连接参数）。
 * 只改了 name 的节点进 `modified` 但不 dirty（出口没变，测速值仍准）。这条包含关系保证了
 * 「测速说 dirty、而 bar 上没有那个节点」在结构上不可能发生。
 */
export interface PendingNodeChanges {
  added: string[];
  modified: string[];
  removed: string[];
  /**
   * 本次运行核起来之后，是否有被「保存不重启」降级、因而**没进核**的结构性变更。
   *
   * 三个节点数组回答不了这件事：`mixedPort` / TUN / DNS 这类改动一个节点都不动，差集恒空却确实
   * 需要重启才生效。条上必须把它算进「有没有东西待应用」，否则「保存」在 UI 上完全无痕。
   */
  restartDeferred: boolean;
}

/**
 * `event:proxyLifecycle` 的载荷：**这一次核起停尝试的真实结局**。
 *
 * Rust 侧真值 = `src-tauri/src/runtime/proxy.rs` 的 `ProxyLifecycleEvent`；三个 phase 各对应一个
 * 可诚实断言的控制流位置（`start_inner` 就绪腿 / `stop_inner` 拆除腿 / `start` 包装的 Err 腿）。
 *
 * # 为什么需要它，以及为什么它不是「差集变空」
 *
 * `event:proxyStarted`/`Stopped` 只由后端**命令层**发，后端自驱的重启（「立即应用」走的正是这条）
 * 一个都不发。而「差集变空」**不能**当成功信号 —— 起核失败时起核快照同样是空、差集同样为空，
 * 拿它判成功会把失败误报成成功。故必须有一条显式携带结局的通道。
 *
 * `errorCode`/`message` 仅 `failed` 时出现；`errorCode` 可能缺失（后端「无可诚实断言的分类」那一类），
 * 但 `message` 恒在 —— 没有它，条只能显示一个说不清的红。
 */
export interface ProxyLifecycleEvent {
  phase: 'ready' | 'stopped' | 'failed';
  errorCode?: string;
  message?: string;
}

/**
 * `config:classifyStaged` 的返回体（暂存层 spec §2.3.4）：候选配置**若现在落盘**会走哪条腿。
 *
 * Rust 侧真值 = `src-tauri/src/runtime/proxy.rs` 的 `StagedClassification`，四个字面量由
 * `ProxyRuntime::classify_staged` 单点产生；跨语言一致性由 `staged-classification.test.ts` 锁死。
 */
export type StagedDecision = 'hotSwitch' | 'noOp' | 'defer' | 'restart';

export interface StagedClassification {
  decision: StagedDecision;
  /** 恒等式：`restartRequired === decision ∈ {defer, restart}`。是 `decision` 的投影，不是第二个判据。 */
  restartRequired: boolean;
}

/**
 * `config:save` 的返回体（暂存层 spec §2.3.3）。Rust 侧真值 = `commands/config.rs` 的 `SaveOutcome`
 * （`#[serde(tag = "status")]` 判别联合）。
 *
 * **`conflict` 不是错误**：IPC 信封仍是 `success:true`，它表达的是「磁盘在你编辑期间被别人改了、
 * 因此一个字节都没写」。走 err 通道会让它和 IO 失败挤在一起，前端只能靠 message 文本分派。
 * 只有传了 `baseVersion` 的调用点才可能拿到它 —— 不传 = 不校验 = 恒 `saved`（今天行为）。
 */
export type SaveOutcome =
  | { status: 'saved'; version: string }
  | { status: 'conflict'; diskVersion: string };

export interface UserConfig {
  subscriptions?: SubscriptionConfig[];

  // 服务器配置
  servers: ServerConfig[];
  selectedServerId: string | null;
  // 最近切换过的节点 id（最近优先，上限 3，直连哨兵不计入）：托盘「节点·最近」MRU 数据源（原型
  // `st.mru`）。由 server_switch 写入（src-tauri/src/commands/server.rs push_recent_server_id）；
  // undefined=旧配置/从未切换过，视作空历史。
  recentServerIds?: string[];

  // 代理模式
  proxyMode: ProxyMode;
  proxyModeType: ProxyModeType;

  // TUN 模式配置
  tunConfig: TunModeConfig;
  // TUN stack 一次性迁移标记（幂等）：存量旧强制默认 stack（mac=gvisor / Win·Linux=system，非真实选择）→ 'auto'。
  // undefined=未迁移（旧配置）；新装由 createDefaultConfig 置 true。详见 ConfigManager.migrateTunStack。
  tunStackMigrated?: boolean;
  // TUN MTU 一次性迁移标记（幂等）：抹掉存量 tunConfig.mtu → 缺席（= 自动）。
  // undefined=未迁移；新装由 default_config 置 true。详见 polaris-store 的 migrate_tun_mtu。
  tunMtuMigrated?: boolean;

  customRules: Rule[];

  // 地区分流（4.1.0）：智能分流 geo 基线场景。undefined=默认中国大陆正向(=今日行为，零迁移)。
  regionRouting?: RegionRoutingConfig;

  // 应用设置
  autoStart: boolean;
  silentStart: boolean;
  autoConnect: boolean;
  minimizeToTray: boolean;
  // 界面语言「选择」（单一真值源）：'auto'（跟随系统）或某受支持语言码（zh-CN/zh-TW/en-US/ru/fa）。
  // 主进程直接据此定托盘/通知语言（不再靠渲染端 IPC 反向告知）；渲染端 i18n 经 additionalArguments 注入同步读取。
  // 可选：存量配置缺该键 → 渲染端首次从旧 localStorage['app-language'] 回填（见 App.tsx 迁移）。
  language?: string;
  autoCheckUpdate: boolean;
  autoLightweightMode: boolean;
  // 图形兼容逃生门（正向语义：**默认开**=true，关闭是 opt-in 用户自救；均需重启生效）。
  // 消费一律用 `!== false`（undefined=未设/旧配置=默认开），对齐本仓 autoCheckUpdate 惯例。
  // hardwareAcceleration：false → 改用软件渲染，规避 GPU 进程反复崩溃/白屏/花屏。
  //   Tauri **没有** Electron `app.disableHardwareAcceleration()` 的等价 API —— webview 的 GPU 开关由各平台
  //   runtime 的环境变量控制，且必须在 webview 创建**之前**设好（判定见 `src-tauri/src/graphics_compat.rs`，
  //   建窗前同步读 config.json 原文本，此刻 store 尚未装配）：
  //     Linux(WebKitGTK)：WEBKIT_DISABLE_DMABUF_RENDERER=1（主修复：NVIDIA 白屏）+ WEBKIT_DISABLE_COMPOSITING_MODE=1
  //     Windows(WebView2)：WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--disable-gpu
  //     macOS(WKWebView)：**无受支持的开关** → 本项在 mac 是 no-op（设置页 mac 隐藏该行，避免死开关）。
  //   生效面 = Linux/Win（与 Electron 版相反：那边因 Electron 在 Linux 无条件禁 HW accel 而藏 Linux；
  //   Tauri/WebKitGTK 合成默认是开的，Linux 上这个开关是活的）。
  hardwareAcceleration?: boolean;
  // windowEffects：门控 macOS vibrancy / Windows Mica（B6「窗口铬」已落地，走 window-vibrancy crate）。
  //   false → 建**不透明**窗（回落 conf 的 backgroundColor:#0B0F14 实色底）且不调特效；transparent 与特效
  //   受同一门控，只摘特效会得到半透明穿透窗（详见 src-tauri/src/graphics_compat.rs 模块头）。
  //   生效面 = mac/Win；Linux(WebKitGTK) 无等价物，特效分支不编译进 Linux → UI 按 data-os="lin" 隐藏该行。
  //   改动需**重启**生效（transparent 是 builder-only、运行期不可改）。
  windowEffects?: boolean;
  desktopNotifications?: boolean; // 桌面通知总开关（默认开）：当前仅严重错误事件发系统通知，关闭则一概不发
  autoUpdateSubscriptionOnStart: boolean; // 订阅自动更新总开关（启动补更陈旧订阅 + 运行期周期更新）
  subscriptionUpdateIntervalHours?: number; // 订阅自动更新周期/陈旧阈值（小时），默认 12
  // 订阅经代理更新的【全局三态策略】（默认 'follow'）：'follow'=按各订阅 per-sub updateViaProxy 决定；
  // 'proxy'=所有订阅强制经代理（忽略 per-sub）；'direct'=所有订阅强制直连（忽略 per-sub）。求值见 shared/subscription-proxy。
  subscriptionProxyPolicy?: SubscriptionProxyPolicy;
  // 全局订阅 UA（被 per-sub subscription.userAgent 覆盖；均缺省时用 `Polaris/<版本>`）。本期无 UI，手编生效。
  subscriptionUserAgent?: string;
  // 更新链路（应用/资源/订阅/核心）是否经代理（默认 true=代理运行时经 update-in 借道，更新源多在 GitHub、
  // 墙内借道更可靠）；false=强制直连。经 shared/update-proxy#resolveMainSessionViaProxy 求值，Phase 1b 后
  // 仅控这四条更新链路；不再影响 default session（已回归 Electron 默认跟随系统代理）。
  mainSessionViaProxy?: boolean;
  rememberWindowSize?: boolean; // 记忆调整后的窗口大小
  enableIPv6?: boolean; // 启用系统全局 IPv6 解析及路由 (不建议开启)
  autoPrivacyMode?: boolean; // 自动进入隐私模式
  privacyPassword?: string; // 隐私模式解锁密码
  autoSwitchNode?: boolean; // 节点故障时自动切换到可用节点
  interruptConnectionsOnSwitch?: boolean; // 切换节点时中断现有连接、强制在新节点重建（默认 true=切换即断旧连接；显式 false=优雅切换，现有连接保留至自然关闭）
  /** 拨号前把目的域名解析成真实 IP 再交给节点（sing-box route action `resolve`）。默认关。
   *  **不与 FakeIP 联动**：mixed 入站无条件生成，经本机代理端口的流量目的地恒以域名交付，
   *  故不存在「本开关必然无效」的配置（判据与取证见 Rust 侧 `resolve_before_dial` 的文档）。 */
  resolveBeforeDial?: boolean;
  // §2 待应用差集：节点配置变更后是否立即重启内核应用（默认 false=OFF=进「待应用」差集、动作条一键应用/被选中时才补全；
  // true=ON=每次节点增/改/删即刻去抖重启，兼作自动订阅刷新的 auto-apply 调度器）。仅节点集/参数变更受此门控，
  // 影响活流量的变更（改选中/被规则指向节点）恒立即重启不受此开关影响。**不入 configGenerationNorm**（纯调度偏好、
  // 不影响 sing-box 配置生成；若入 norm 则切开关本身会触发无谓重启）。
  restartOnNodeChange?: boolean;
  // 组网登录期出口让位（默认开=undefined||true）：当选中出口为 Tailscale 且其隧道尚未 Running 时，默认路由临时让位
  // 直连（hotSwitchSelector→direct，零重启），使浏览器授权页/引导期控制平面流量不被导向「尚未连上的 TS 出口」而死锁；
  // 隧道 Running 后自动切回 TS 出口。关闭=false：宁可授权失败也不在引导期直连（无泄漏窗口）。见 shared/mesh-login-fallback。
  meshLoginFallbackDirect?: boolean;

  // 窗口尺寸（仅在 rememberWindowSize 启用时使用）
  windowBounds?: { width: number; height: number };

  dnsConfig?: DnsConfig;

  customRuleSets?: CustomRuleSet[];

  // 已下载的本地规则资源（.srs）
  ruleResources?: RuleResource[];

  // GitHub 加速前缀（规范化 'https://host[:port]/'；''/undefined=直连，默认）。仅规则资源下载用。
  ghProxyPrefix?: string;

  // 已下载规则资源自动更新总开关（默认 false）。本地 .srs sing-box 不会自更新，需 Polaris 周期重下载。
  ruleResourceAutoUpdate?: boolean;
  // 自动更新间隔（小时）：6|12|24|72|168，默认 24
  ruleResourceUpdateIntervalHours?: number;
  // 内置 geo 规则集（geosite-cn 等）最近一次网络更新时间，按 tag 索引。无记录=出厂版（视为可补更）。
  builtinGeoMeta?: Record<string, { updatedAt?: string }>;

  // 应用分流规则（实验性）
  appRules?: AppRule[];
  // 应用分流总开关；undefined=true（兼容老配置，升级不静默失效）。false → appRules 完全不进 route 生成/TUN 排除/geo 收集。
  appRoutingEnabled?: boolean;
  // 应用分流默认预设一次性注入标记：首次为内置预设注入默认「代理·跟全局」规则后置 true。
  // 防每次启动回灌（用户删掉某预设规则后不会被重新补回）。新装由 createDefaultConfig 直接置 true。
  appRulesSeeded?: boolean;

  customAppPresets?: CustomAppPreset[];

  // 端口配置
  // `socksPort` 不在此声明：mixed-only 架构下它既不是 Rust `UserConfig` 字段（不在 `FIELD_NAMES`），
  // 也无任何读取点 —— `crates/store/src/migrate.rs:94` 载盘时**直接删除**它，从不读值。
  /** @deprecated mixed-only 架构已弃用独立端口；仅作旧配置迁移种子读取（见 `user_config/proxy_ports.rs`）。 */
  httpPort?: number;
  // 本地混合代理端口（mixed inbound 同口 HTTP+SOCKS）。mixed-only 架构 canonical 端口，恒 >0、无开关。
  // 新装缺省 7890（DEFAULT_MIXED_PORT，对齐业内）；旧配置迁移自 httpPort。统一经 shared/proxy-ports#localProxyPort 取用。
  mixedPort?: number;
  // clash_api 外部控制端口。新装缺省 9090（DEFAULT_CONTROL_PORT，对齐业内）。可改：当 9090 被另一 clash 系应用/
  // 任意进程占用时，Polaris 据此改 external_controller 端口，避免 clash_api 无法 bind 的死局。统一经 shared/proxy-ports#controlApiPort 取用。
  controlPort?: number;
  allowLan?: boolean; // 局域网共享代理（允许其他设备连接）
  bypassLAN?: boolean; // 绕过局域网总开关（关 → 不绕过，内网/清单内目标也走代理）
  // 绕过局域网完整清单（用户可编辑，含 CIDR + 域名）：undefined=用 DEFAULT_BYPASS_LAN（字节不变）；已编辑存全量。
  // 受 bypassLAN 开关管理。双消费：① TUN 模式 route 私网直连 + Windows TUN 排除只取其 CIDR（bypassLanCidrs，
  // mac/Linux 走 route 规则规避 NetworkExtension HANG）；② 系统代理模式整份（CIDR+域名）写入 OS 忽略列表。
  // 原 systemProxyBypass 已并入此字段（单一清单，杜绝两处重复）。
  bypassLANList?: string[];
  blockQuic?: boolean; // 阻止 QUIC（对代理向 UDP 443 执行 reject，逼浏览器回退 TCP）；默认关；节点无关，对所有协议一视同仁
  tlsFragment?: boolean; // 全局 TLS 分片：对所有 TLS 节点切分 ClientHello 抗 SNI-DPI；默认关
  // WebRTC 防泄露（仅 TUN 模式生效）：off=关；proxy=对 STUN 经协议嗅探强制走代理（srflx=代理 IP，WebRTC 正常）；
  // block=reject STUN（断 P2P、零公网 IP 泄露）。默认 undefined=off（零行为变化）。系统代理模式拦不住（浏览器 WebRTC
  // 的 UDP 不经 sing-box 核），UI 在系统代理模式置灰。
  webrtcLeakProtection?: 'off' | 'proxy' | 'block';
  // 节点测速端点 URL（经各节点代理 GET 量 TTFB）。默认 generate_204（见 contracts/speed-test）；用户可自配，兼容 http/https。
  // 非法值由 SpeedTestService 经 resolveSpeedTestTarget 回落默认，不阻断测速。
  speedTestUrl?: string;
  // fake-ip-filter 默认清单（NTP/STUN/Captive 等在 FakeIP 下会坏的域名 → 真实解析）。默认开（undefined/true=开），仅 false=关。
  fakeIpFilter?: boolean;
  // FakeIP 例外域名清单（用户可编辑）：undefined=用内置默认（DEFAULT_FAKEIP_FILTER_DOMAINS，与历史字节一致）；
  // 已编辑则存全量。仅 fakeIpFilter 开时生效。内置 captive 域名仍走内网解析器，其余走 dns-domestic（见 singbox-dns-builder）。
  fakeIpFilterList?: string[];
  /** 拦截浏览器内置 DoH（Chrome/Firefox「安全 DNS」）总开关；默认关。 */
  blockBrowserDoh?: boolean;
  /** 被拦的 DoH 端点域名清单（domain_suffix 语义）。未编辑=undefined → 用内置起点。 */
  browserDohList?: string[];
  // 核心更新：仅在配置生成器已验证的 sing-box minor 版本带内自动更新（默认 true）。关闭后允许自动
  // 更新跨越 minor（如 1.13→1.14），但跨 minor 的 schema 变更可能导致配置不兼容、需手动处理。
  restrictCoreUpdateToCompatibleMinor?: boolean;
  // 内核自动更新总开关（默认 false；读取端 === true 判定，不进 createDefaultConfig）。开启后调度器周期检查、
  // 仅在「同 major.minor 兼容版本带内」（如 1.13.x→1.13.y）自动下载+预检+落位；跨 minor 一律不自动，仅提示。
  // 落位永远只在代理进程不存在时发生（运行中暂存 staged，延到停止/启动/用户点立即应用），绝不静默断流。
  autoUpdateCore?: boolean;
  /**
   * sing-box 内核更新通道。缺省 / 非法值均按 `'stable'`（= 本字段引入前写死的行为）。
   *
   * `'prerelease'` 才把 GitHub 上 `prerelease=true` 的 release（sing-box 的 alpha/beta/rc）纳入候选。
   * **不按 alpha/beta/rc 再细分**：GitHub 只给一个布尔，档次仅存在于 tag 文本里，靠字符串猜档
   * 会在上游改命名的那天静默失效。
   *
   * 与 `restrictCoreUpdateToCompatibleMinor` **正交**：本字段决定「看不看预发布」，那个决定
   * 「跨不跨 minor」。开了预发布不会顺带放开跨带闸。
   */
  coreUpdateChannel?: 'stable' | 'prerelease';
  // **App**（非内核）更新的自动下载开关（默认 false；读取端 === true 判定）。开启后「检查到新版本」应在后台
  // 自动 update_download，就绪后提示安装。与 `autoUpdateCore` 是两件事：那个管 sing-box 内核二进制的换核，
  // 这个管 Polaris 自身安装包的下载 —— 此前 UI 把「发现新版本时自动下载」错绑到 autoUpdateCore，
  // 用户拨的是内核开关却以为在管 App 下载（语义错位，本批拆开）。
  // ⚠️ 后端消费者尚未接线（全仓无读取点，调度腿由更新域另批补）：当前仅持久化用户意愿，UI 如实标注。
  autoDownloadUpdate?: boolean;
  /** @deprecated 已迁移至 customRules（processName+direct）；仅保留兼容旧配置，ConfigManager 启动时清空迁移。 */
  bypassProcesses?: string[];

  // 日志设置
  logLevel: LogLevel;
  // 关闭日志写盘（sing-box log.disabled）；默认 false=写盘。关闭后应用内无法查看实时日志/基于日志的诊断
  disableLogFile?: boolean;

  // 此处曾有 diagnosticCapture（临时把 logLevel 提到 debug 并存档原级别）。整条机制已删除：
  // 内核日志改由管理 API 的 SubscribeLog 全级别送来、级别筛在客户端，改级别即刻生效且不重启内核。
  // 旧配置里的残留键由后端迁移链（store::migrate::migrate_diagnostic_capture）还原级别后清除。

  // clash_api(127.0.0.1:9090) 鉴权 secret：首次启动随机生成并持久化，统一管所有 clash_api 访问(含 external_ui)。
  // 内部调用(热切换/流量统计/拓扑)带 Authorization；防恶意网页跨域读连接历史。可在设置里查看/重置。
  clashApiSecret?: string;

  // sing-box 1.14 官方面板（opt-in 逃生舱）：开关 on 时在 api service（management api）注入 dashboard.enabled，
  // 核首次联网从官方仓拉 sing-box-dashboard 资源并于 api 监听口 /dashboard/ serve。默认关（undefined=false）；
  // 不随包静态文件，关时核绝不出网拉资源。非 boolean 一律 sanitize 删除（同 appRoutingEnabled 标准，不回填默认）。
  singboxDashboard?: boolean;

  // sing-box 官方面板资源 download_url 覆盖（可选）：空/未设 = 用内置默认（SagerNet/sing-box-dashboard gh-pages.zip）。
  // 下发前经 ghProxyPrefix 镜像加速包裹；改此值 → saveConfig 检测变更后删本地缓存目录，使核下次启动重拉新资源。
  // 暴露给用户作安全阀：默认 URL 失效时可手动改。空白字符串/非字符串一律 sanitize 删除（回落默认）。
  singboxDashboardUrl?: string;

  // macOS 提权 helper：用户已忽略「安装提权 helper」启动提示（不再每次启动 TUN 时弹）。可在设置里重新安装。
  // 注：socket 鉴权 token 刻意不放这里——它存独立文件，避免被渲染端整体回写 config 时清零。
  helperPromptDismissed?: boolean;

  // 提权 helper：用户已忽略「helper 可升级」启动弹窗（proto < 期望，如属主根治 v6）。不再每次启动提示；设置页可手动升级。
  helperUpgradePromptDismissed?: boolean;

  // macOS 提权 helper：用户已忽略「后台运行被系统禁用」引导弹窗（不再提示）。设置页 helper 卡保留常驻入口。
  helperDisabledPromptDismissed?: boolean;

  uiTheme?: 'light' | 'dark' | 'system';
}
