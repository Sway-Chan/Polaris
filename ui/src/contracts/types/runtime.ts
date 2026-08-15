import type { LogLevel } from '../types';
/**
 * `proxy:getStatus` 返回 / app-store `proxyStatus`。**逐字段与 Rust `runtime/proxy.rs::ProxyStatus`
 * DESIGN-REVIEW(proxystatus-currentserver-removed)：`currentServer` 已删——契约/registry/系统设计
 * 三处均无此字段（仅 TS 侧历史声明），前端真值 = store 的 servers + selectedServerId。待复审确认。
 *
 * 对账**（此前 startTime/uptime/errorCode 三字段后端从不下发 → 恒 undefined，`currentServer` 则是
 * 冗余声明——Home「运行时长」因此无源。新增字段时两侧必须同改，否则又变成「契约声明了、后端没接」。
 */
export interface ProxyStatus {
  running: boolean;
  pid?: number;
  /** 起核**就绪**时刻（epoch ms；未运行则缺省）。运行时长的唯一真值 —— 要让时长自己走字，
   *  用它在渲染端本地 tick，别依赖 `uptime`（理由见该字段）。 */
  startTime?: number;
  /** 已运行秒数，后端在 `getStatus` **应答那一刻**现算。
   *  ⚠️ 是快照值、不会自己走字：`proxyStatus` 只在挂载 + proxyStarted/proxyStopped 事件时刷新
   *  （`App.tsx` / `app-store.refreshProxyStatus`，无轮询）→ 读它渲染的时长会停在刷新那一刻。
   *  活的时长请用 `startTime` + 本地定时器。 */
  uptime?: number;
  /** 最近一次错误消息。仅供展示/日志，**禁止用于分类** —— 分类用 `errorCode`。 */
  error?: string;
  /** 最近一次错误的结构化码。与 `event:proxyError` 同源同点落值（Rust `set_error`）→
   *  错过事件的界面仍可从状态读到码。后端只发它能从控制流位置诚实断言的成员：
   *  `STARTUP_FAILED`（起核腿失败）/ `PROCESS_EXITED`（核意外退出且无法自愈）/
   *  `AUTO_RESTART_FAILED`（自愈达上限放弃）。其余成员本仓暂无分类器可产出。 */
  errorCode?: ProxyErrorCode;
  /** 运行期 HTTP/SOCKS 混合入站端口（未运行=0）。 */
  mixedPort?: number;
  /** 管理 API 端口。字段名沿用 `clashApiPort` 与后端契约一致，但**语义已是 sing-box 1.14
   *  `services:[{type:'api'}]` 的 h2c gRPC 监听口**，非历史 clash REST 端口。 */
  clashApiPort?: number;
  /** 是否经 helper 提权启动（macOS 路径）。 */
  startedViaHelper?: boolean;
  /** **是否有起核腿在飞**（后端读时投影；false 时后端省略该字段，故是可选）。
   *
   *  `running:false` 期间也可能正在起核 —— 起核重试预算内一轮可达数十秒。托盘浮层是独立窗口、
   *  不共享主窗 store，只能靠它得知「此刻正在启动」，从而把「连接」换成「取消」；缺了它就会在
   *  已有起核腿之上再叠一次 start。 */
  starting?: boolean;
}

// ============================================================================
// 代理错误码协议（跨进程错误分类的唯一依据；message 仅供展示/日志，禁止用于分类）
// 成员从 ProxyManager 现有 includes()/退出码检测逐条反推，string enum 保证 wire 稳定可 grep。
// ============================================================================

export enum ProxyErrorCode {
  // 连接类 → ErrorCategory.Connection
  DEST_CONNECTION_REFUSED = 'DEST_CONNECTION_REFUSED', // 'report handshake success: connection refused'
  CONNECTION_REFUSED = 'CONNECTION_REFUSED', // 'connection refused'
  CONNECTION_TIMEOUT = 'CONNECTION_TIMEOUT', // 'timeout'|'timed out'
  DNS_RESOLVE_FAILED = 'DNS_RESOLVE_FAILED', // 'dns'+'fail'
  TLS_CERT_ERROR = 'TLS_CERT_ERROR', // 'certificate'|'tls'|'ssl'（排除 anytls/shadowtls）
  AUTH_FAILED = 'AUTH_FAILED', // 'authentication failed'|'auth fail'
  // 配置类 → ErrorCategory.Config
  CONFIG_INVALID = 'CONFIG_INVALID', // 'invalid config'|'config error'、退出码 2
  PORT_IN_USE = 'PORT_IN_USE', // 'address already in use'
  CLASH_API_PORT_RECYCLING = 'CLASH_API_PORT_RECYCLING', // 9090 处于 TIME_WAIT 回收中（瞬态，自动等待，非终态）
  // 权限/环境类 → ErrorCategory.System
  PERMISSION_DENIED = 'PERMISSION_DENIED', // 'permission denied'|'access denied'
  SYSTEM_PROXY_FAILED = 'SYSTEM_PROXY_FAILED', // 核心已起但系统代理 networksetup/reg 设置失败（非终态提示）
  EXIT_MISMATCH = 'EXIT_MISMATCH', // 核在跑但实际生效出口 ≠ 用户选中节点（静默直连风险，非终态；见 proxy.rs::attest_effective_exit）
  RULE_RESOURCES_MISSING = 'RULE_RESOURCES_MISSING', // 核在跑但本地 .srs 缺失/损坏 → 引用它的分流规则被 fail-closed 剪枝（分流降级、未命中规则兜底走代理而非静默直连，非终态；见 proxy.rs::warn_pruned_rule_resources 与 App.tsx 能力降级腿注释的两个例外）
  BINARY_NOT_EXECUTABLE = 'BINARY_NOT_EXECUTABLE', // 退出码 126
  BINARY_NOT_FOUND = 'BINARY_NOT_FOUND', // 退出码 127
  CRONET_LIB_MISSING = 'CRONET_LIB_MISSING', // 'cronet: library not found' / dlopen 失败（naive 出站缺/坏 libcronet → 整核 FATAL，自愈冷路径触发）
  HELPER_NOT_INSTALLED = 'HELPER_NOT_INSTALLED', // TUN 需提权 helper 但未安装、且引导门内也没能装上 → 渲染端引导去「设置 › Helper」，不抛裸 socket ENOENT
  HELPER_GATE_ABORTED = 'HELPER_GATE_ABORTED', // TUN 提权引导门弹出后用户点了「取消」→ 中性告知「已取消、未启动」，**不得**复用 HELPER_NOT_INSTALLED 的「去安装」引导（用户刚拒绝过，再催一遍是无视其选择）
  TUN_ROUTE_NOT_CAPTURED = 'TUN_ROUTE_NOT_CAPTURED', // TUN 模式起核就绪后 post-flight 判定「应走代理的公网目的」出口 grace 内始终未从 baseline 切走（其他 VPN 占默认路由 / 我方路由装失败）→ 硬闸拒绝标 connected，避免「假报已连接、连接数 0」（见 proxy.rs::verify_tun_route_captured）
  TUN_ADDRESS_UNAVAILABLE = 'TUN_ADDRESS_UNAVAILABLE', // #332 核 stderr 的 FATAL 行指明失败发生在「给 TUN 网卡装地址」这一步（地址被残留网卡/其他 VPN 占用）→ 专属真因上屏，替代「起核超时/启动期退出」这种与现场无关的通用失败（判据是 sing-box/sing-tun 源码字面量，不是 errno 文案；见 proxy.rs::classify_core_fatal_line）
  TUN_ADAPTER_MISSING = 'TUN_ADAPTER_MISSING', // #327 TUN@Windows 起核就绪后逐腿正向验证：整个重试预算内一次都没枚举到本次配置的 wintun 适配器（网卡压根没建出来）→ 硬闸拒绝标 connected。与 TUN_ROUTE_NOT_CAPTURED 分工：那条是「建出来了但路由被别的 VPN 占」（去断开对方），本条是「根本没建出来」（去查 wintun 驱动/安全软件拦截），指引相反不得合并（见 proxy.rs::probe_tun_adapter_present）
  ROOT_ORPHAN_BLOCKED = 'ROOT_ORPHAN_BLOCKED', // 上次遗留的 root 孤儿核用户态杀不动、独占 cache.db，任何模式都起不来 → 阻断起核并落终态；message 携带该孤儿的 pid（proxy.rs::escalate_root_orphans），是用户唯一能据以行动的信息
  // 进程生命周期类 → ErrorCategory.Process
  STARTUP_FAILED = 'STARTUP_FAILED', // 退出码 1
  PROCESS_KILLED = 'PROCESS_KILLED', // 退出码 137
  PROCESS_EXITED = 'PROCESS_EXITED', // 其它异常退出
  AUTO_RESTARTING = 'AUTO_RESTARTING', // 自动重启中（瞬态）
  AUTO_RESTART_FAILED = 'AUTO_RESTART_FAILED', // 自动重启失败达上限
  RESTART_LIMIT_REACHED = 'RESTART_LIMIT_REACHED', // 健康检查发现死亡且重启耗尽
  STOP_AUTH_CANCELLED = 'STOP_AUTH_CANCELLED', // 停止时用户取消提权授权、进程仍在运行（非终态）
  CORE_UPDATE_IN_PROGRESS = 'CORE_UPDATE_IN_PROGRESS', // 内核二进制替换窗口中，手动 start/restart/switchMode 被拒（瞬态，非终态）
  UNKNOWN = 'UNKNOWN',
}

/** 渲染端信任前的运行时校验（防 errno 串等任意 .code 混入误判）。 */
export function isProxyErrorCode(v: unknown): v is ProxyErrorCode {
  return typeof v === 'string' && (Object.values(ProxyErrorCode) as string[]).includes(v);
}

/** EVENT_PROXY_ERROR 统一 payload。新增字段全 optional → 旧渲染端零破坏。 */
export interface ProxyErrorEvent {
  message: string; // 【兼容】已合成的展示串，旧渲染端继续可用
  errorCode?: ProxyErrorCode; // 【新增】结构化分类，渲染端优先消费
  errorParams?: Record<string, string | number>; // 【新增】i18n 插值参数
  code?: number; // 【兼容】进程退出码语义
  signal?: string | null; // 【兼容】
  error?: string; // 【兼容·deprecated】原始 raw
}

/**
 * 启动前配置校验 gate 剔除的非法节点（坏节点拖垮 sing-box 整体启动 FATAL → 启动前 check 剔除）。
 * 仅会话内存语义：每次启动重判，换核自动复活；reason 区分「直接被 check 标中」/「detour 级联剔除」。
 * 经 EVENT_PROXY_INVALID_NODES 推送渲染端，节点列表据此标灰 + tooltip（不禁用点击）。
 */
export interface InvalidNodeInfo {
  id: string;
  tag: string;
  reason: string;
}

// ============================================================================
// 连接快照（topology 统一供数：main 1s 轮询 clash_api /connections 留存裁剪后推送）
// ============================================================================

/**
 * clash /connections 单条连接（main 裁剪后子集）。
 * topology 只用 id/chains/rule/rulePayload/metadata{host,destinationIP}；连接信息页额外用扩展字段
 * （network/type/sourceIP/sourcePort/destinationPort/processPath + upload/download/start）算速率/源/进程/时长。
 * 扩展字段全 optional → 向后兼容 topology（拿到更多字段但只读原有的）；含 sourceIP/processPath 隐私字段，
 * 故连接信息页须在隐私模式下屏蔽明细（见 connections-page）。
 */
export interface ConnectionEntry {
  id: string;
  chains: string[];
  rule: string;
  /**
   * 规则载荷。**当前无生产者**：上游 gRPC `Connection`（`stats-engine/src/types.rs` 的
   * `SingBoxConnection`）只有 `rule` 一个字段，没有 payload。声明成必填是当年照抄 clash HTTP API
   * 契约留下的空头支票 —— 消费方（连接页规则列）已用可选链兜住，但类型必填会让下一个人以为它一定在。
   * 哪天上游补了这个字段，把 `trim_connection` 一并接上再改回必填。
   */
  rulePayload?: string;
  metadata?: {
    host?: string;
    destinationIP?: string;
    network?: string; // tcp/udp
    type?: string; // 入站类型（如 Tun/HTTP/Socks）
    sourceIP?: string;
    sourcePort?: string;
    destinationPort?: string;
    processPath?: string; // 发起连接的进程路径（隐私字段）
  };
  upload?: number; // 累计上行字节
  download?: number; // 累计下行字节
  start?: string; // 连接建立时刻（RFC3339）
}

/** 连接快照：连接信息页订阅 'detail' topic（batch3 §3.7）——订阅即回初始帧 + worker 每帧 push，仅页面打开且可见时传（无订阅者 → worker 逐级停机），非旧「每秒全量广播给所有窗口」。 */
export interface ConnectionsSnapshot {
  connections: ConnectionEntry[];
  at: number; // 采样时刻 epoch ms
}

/** 已结束连接；closedAt 为 sing-box UnixNano。 */
export interface ClosedConnectionEntry {
  entry: ConnectionEntry;
  closedAt: number;
}

/** 已结束连接历史快照：最新在前，后端最多保留 1000 条。 */
export interface ConnectionsClosedSnapshot {
  connections: ClosedConnectionEntry[];
  at: number;
}

/**
 * 已结束历史事件：首帧/reset 帧是完整快照，常态帧只含本批 upsert 和淘汰 id。
 * 前端在本地维护最多 1000 条，避免每新结束一条就重复传输/投影全部历史。
 */
export interface ConnectionsClosedUpdate {
  reset: boolean;
  connections: ClosedConnectionEntry[];
  removedIds?: string[];
  at: number;
}

/** 拓扑「其它」分组的 sentinel host 名（聚合 Top-N 截断后剩余的合并条目）。渲染端 topology-layout 见此值 →
 *  替换为 i18n 文案 t('home.others')。用控制字符前缀确保绝不与真实 host/IP/rule 名冲突。 */
export const TOPOLOGY_OTHERS_KEY = '\u0000others';

/** 某目标（host）→ 单个出口的连接数分布项。 */
export interface ConnectionAggFlow {
  outbound: string;
  count: number;
}

/** 按目标聚合的一组连接：host/destIP/rule 显示名 → 连接数 + 各出口分布（topology 中列节点）。 */
export interface ConnectionAggHost {
  name: string;
  count: number;
  flows: ConnectionAggFlow[];
}

/** 按出口聚合的连接数（topology 右列节点）。 */
export interface ConnectionAggOutbound {
  name: string;
  count: number;
}

/**
 * 连接聚合快照（首页拓扑专用）：StatsWorkerHost 每帧 O(N) 聚合后经 EVENT_CONNECTIONS_AGGREGATE 广播，载荷与连接
 * 总数解耦（恒 ~Top-N host + 出口数）。取代「每秒全量 ConnectionEntry[] relay」——连接风暴下渲染端不再被全量明细
 * 序列化 + 每秒 O(N) 重算拖死（issue #227）。hosts 已按 count 降序、截断 Top-N（剩余并入 TOPOLOGY_OTHERS_KEY）。
 */
export interface ConnectionsAggregate {
  total: number; // 活跃连接总数
  hosts: ConnectionAggHost[];
  outbounds: ConnectionAggOutbound[];
  at: number; // 采样时刻 epoch ms
}

// ============================================================================
// macOS 提权 helper 状态
// ============================================================================

export interface HelperStatus {
  /** 当前平台是否支持提权 helper（Polaris 统一 helper：mac/win/linux 三平台均支持，未知平台为 false） */
  supported: boolean;
  /** helper 二进制 + LaunchDaemon plist 是否在位 */
  installed: boolean;
  /** socket ping 成功且协议版本 ≥ 最低可用（可零提权驱动 TUN） */
  ready: boolean;
  /** 可用但有新版 helper（v5 install-core）：proto ≥ 最低可用但 < 期望 → 温和提示可升级（非故障，不强制重装） */
  upgradeable: boolean;
  /** 协议版本（ping/version 返回），未就绪为 null */
  version: string | null;
  /** daemon 是否被 launchd 加载（launchctl print 退出码）；非 macOS / 未安装为 null */
  loaded: boolean | null;
  /** 已安装但无法就绪、协议版本不符、或烧录路径与当前 app 不符 → 建议重装修复 */
  needsRepair: boolean;
  /** macOS「系统设置→登录项→允许在后台」被关。判定链：SMAppService.statusForLegacyURL(=2) → BTM disposition 直读
   *  → launchctl 去抖启发式（BTM .btm 目录受 TCC 完全磁盘访问保护、生产 GUI 读不到，故 SMAppService 为权威首通道）。
   *  可与 ready=true 并存（install-over-top 混合态）。消费方契约：先判 backgroundDisabled 再判 needsRepair/pathMismatch。 */
  backgroundDisabled: boolean;
  /** 仅 macOS 打包版：plist 烧录的 sing-box 路径 ≠ 当前 app 路径（app 被移动过） */
  pathMismatch: boolean;
  /** plist 中烧录的 sing-box 路径（诊断展示用；未装/解析失败为 null） */
  installedSingboxPath: string | null;
}

/**
 * `system_proxy_get_status` 返回（Rust `commands/proxy.rs::SystemProxyLiveResponse`）——
 * **当前 OS 代理设置的活态快照 + 「是否仍指向本进程 mixed 入站」的判定**。
 *
 * # 消费纪律：读 `pointsToUs`，不要自己拿 `enabled` 判
 *
 * `enabled` 只说明 OS 层开着代理，指向的可能是**别人**（第三方代理软件 / 用户手配 / 端口被改）——
 * 那时我们的流量同样没经本地核，UI 若据 `enabled` 亮绿灯就是审计里那条「绿灯 + 流量不经核」的误导。
 * 判定统一由后端做（比对 `127.0.0.1:<mixedPort>`，见 `expected`），前端只读结论。
 *
 * # 为什么需要它（前端补不了的两条漏报腿）
 *
 * 起核那一刻的 `ProxyStatus.errorCode = SYSTEM_PROXY_FAILED` 测不出：①运行期用户在系统设置里手动
 * 关掉/改掉代理（起核时是成功的）；②`errorCode` 是单槽，后来的非终态错误会把它覆盖掉。本查询读的是
 * **此刻的 OS 设置**（地面真相），对两条是同一个根治。用法见 `screens/home/connection-state.ts`。
 */
export interface SystemProxyStatus {
  /** OS 层是否启用了系统代理。**不是**「是否指向我们」——判定读 `pointsToUs`。 */
  enabled: boolean;
  httpProxy?: string;
  httpsProxy?: string;
  socksProxy?: string;
  /** 当前 OS 代理是否仍指向本进程的 mixed 入站（= `expected`）。**本接口的判据本体**。 */
  pointsToUs: boolean;
  /** 比对基准 `127.0.0.1:<mixedPort>`（诊断展示用）。 */
  expected: string;
  /** 后端当前不下发（enable 侧才有 bypass 清单；活态读取面不回读它）。 */
  bypassList?: string[];
}

export interface LogEntry {
  timestamp: string;
  level: LogLevel;
  message: string;
  source: string;
  stack?: string;
}

/**
 * 流量统计快照（`EVENT_STATS_UPDATED` 载荷，'stats' topic）。
 *
 * 供数：后端一条 `SubscribeStatus` 长驻流（`runtime/stats.rs` 的 `run_stats_stream`），约 1s 一帧。
 * Rust 侧是 `polaris_stats_engine::TrafficStats`，键名由该结构的 `rename_all = "camelCase"` 保证
 * 与本接口逐字一致（两侧各有一条键名契约测试守着）。
 */
export interface TrafficStats {
  /** 上行速率 bytes/s。后端对 totalUpload 做跨帧差分 ÷ 实测 Δt 求得；**每条流的首帧恒为 0**（无基线）。 */
  uploadSpeed: number;
  /** 下行速率 bytes/s，口径同 uploadSpeed。 */
  downloadSpeed: number;
  /**
   * 累计上行字节，口径 = **本次核启动至今单调累加**（内核 `trafficManager` 的只增计数器）。
   *
   * 单调只在**一条核生命线内**成立：停核 / 换核 / 换节点重启核之后从 0 重新开始，届时会看到它
   * 「变小」——那不是回退，是新的一条生命线。要展示「历史总用量」得自己在前端跨生命线累加，
   * 别把本字段当那个用。
   */
  totalUpload: number;
  /** 累计下行字节，口径同 totalUpload。 */
  totalDownload: number;
  /**
   * 活跃连接数（内核 `Status.connectionsIn` = `trafficManager.ConnectionsLen()`）。
   *
   * ⚠️ 与首页拓扑 / 连接明细页那个总数**口径略有不同**：那两处走连接事件流，会滤掉应用自己的
   * 测速探测连接；本字段是内核未过滤的口径，测速期间可能略大。
   */
  activeConnections?: number;
}

// ============================================================================
// 出口 IP 信息（本地直连出口 / 代理出口）
// ============================================================================

export interface IpInfo {
  ip: string;
  country?: string;
  countryCode?: string;
}

/** TS 出口 API 直判无效、不探测的终态原因（非空=未认证 / 未选出口设备 / exit peer 离线 / exit peer 在线但未广告出口）。 */
export type ProxyExitBlock =
  | 'ts-needs-auth'
  | 'ts-no-exit-device'
  | 'ts-exit-device-offline'
  | 'ts-exit-not-advertised';

export interface IpInfoSnapshot {
  /** 本地直连出口（auto_detect_interface 物理网卡），代理未连时也可测。 */
  direct: IpInfo | null;
  /** 代理出口（当前选中节点），代理未连时为 null。 */
  proxy: IpInfo | null;
  updatedAt: number;
  loading?: boolean;
  error?: string;
  /** TS API 直判出口无效、不探测的终态；非空=选中 TS 出口未广告 / exit peer 离线。状态栏据此显「出口无效」，
   *  真探测发生（proxy 探到值或走真链探测）即清空——与 error（探测失败）互斥语义：blocked=没探（已知无效）。 */
  proxyBlocked?: ProxyExitBlock;
}

export interface ApiResponse<T = any> {
  success: boolean;
  data?: T;
  error?: string;
  code?: string;
}

export interface AutoStartStatus {
  enabled: boolean;
  path?: string;
}

export interface PlatformInfo {
  platform: NodeJS.Platform;
  arch: string;
  version: string;
  isAdmin: boolean;
}
