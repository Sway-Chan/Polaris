/**
 * 解锁检测 —— main/renderer 共用类型 SoT（单一真值）。
 *
 * 为何在 shared：main 侧检测引擎与 renderer 侧展示共用同一套状态枚举/结果结构，双份定义必漂移
 * （见 polaris-unlock-detection.md §2.2）。renderer 的 `unlock-service-config.ts` re-export 本文件的
 * `UnlockStatus/UnlockResult`，不另定义。
 */

/**
 * 检测状态。
 * - `idle`：初始/未检测/gating（代理未运行）态。
 * - `checking`：编排器/hook 置位（检测在飞）。
 * - `ok`：可解锁（Netflix 语义下 = 非自制内容也可看）。
 * - `partial`：部分解锁（Netflix = 仅自制剧；Disney = inSupportedLocation:false）。
 * - `blocked`：命中**显式地区封禁 marker** 才判（地区不提供 → 换国家/地区节点）。
 * - `restricted`：检测请求被**风控/反爬拦截**（CF 挑战 / 1020 / IP 信誉），**非地区问题**。语义 ≠「用户被封」：
 *   挑战对真浏览器通常自动过 → 换 IP 质量更好的出口节点（同国家即可），实际可用性以浏览器访问为准。
 * - `timeout`：网络错误/超时/无法判定的统一兜底。
 */
export type UnlockStatus = 'idle' | 'checking' | 'ok' | 'partial' | 'blocked' | 'restricted' | 'timeout';

/** 单个服务的检测结果。region = 该服务边缘节点判定的地区码（ISO-3166-1 alpha-2，可缺）。 */
export interface UnlockResult {
  status: UnlockStatus;
  region?: string;
}

/**
 * 检测**已实现**的服务 id 全集（顺序即编排/展示顺序）——注意「已实现」≠「已上线」，上线集见
 * [`ENABLED_SERVICE_IDS`]。本数组同时是 `ServiceId` 联合类型的来源，故停飞的服务也留在这里。
 *
 * **必须与 Rust `crates/unlock/src/types.rs` 的 `ServiceId::ALL` + `ServiceId::PENDING_CALIBRATION`
 * 并集逐一同序**。双向锁由**两条**测试合成，各锁一个方向：
 *  - Rust `service_ids_order_matches_frontend` / `service_ids_are_subsequence_of_frontend` /
 *    `shipped_plus_pending_covers_frontend_service_ids` —— 期望值是本文件数组的**硬编码副本**，
 *    锁「改 Rust 忘了改这里」；
 *  - `./unlock-detection.test.ts` —— **直接读 Rust 源码**解析那两个常量再比对，锁「改这里忘了改 Rust」。
 *
 * 首页按 `UNLOCK_SVCS.grp` 分两行：AI 行 chatgpt/claude/gemini，流媒体行 netflix/disney/tiktok/spotify。
 */
export const SERVICE_IDS = ['chatgpt', 'claude', 'gemini', 'grok', 'netflix', 'disney', 'tiktok', 'spotify'] as const;
export type ServiceId = (typeof SERVICE_IDS)[number];

/**
 * 【前端开关 · 与 Rust `ServiceId::PENDING_CALIBRATION` 镜像】已实现但**未上线**的服务：checker/端点/
 * 单测全在仓里，只是判据强度不够、不进用户可见集——既不渲染徽章，后端也不为它发探测。
 *
 * 当前唯一项 `grok`：判据是**弱检测**（站点可达 + 风控拦截 + 出口国家码），且 G3 地区封禁规则为**空**
 * （永不判 `blocked`）→ 受限地区会被显示成「已解锁」，风险方向是**谎报绿灯**。决策见
 * `~/docs/polaris/design/polaris-unlock-calibration.md:49-52`：「别用可达性冒充解锁检测」。
 *
 * **启用前置条件**：真机哨兵标定（`polaris-unlock-challenge-and-grok.md` §10 T7 —— US/EU 双出口
 * `POST /rest/models` 的 modelId 差集定出稳定哨兵）→ 回填 G3′ 强规则 → 才把 `'grok'` 从本数组删掉，
 * 并同步把 `ServiceId::Grok` 从 Rust `PENDING_CALIBRATION` 移回 `ServiceId::ALL`。
 *
 * ⚠️ **只删这里不改 Rust** = 徽章开始渲染 grok，而后端 `detect_all` 只遍历 `ServiceId::ALL` ⇒ 一次探测
 * 都不发 ⇒ 徽章恒 idle。这个方向由 `./unlock-detection.test.ts` 转红拦下（Rust 侧那几条契约测试比对的
 * 是硬编码副本，**看不见**本文件的改动）。
 */
export const PENDING_CALIBRATION_SERVICE_IDS: readonly ServiceId[] = ['grok'];

/**
 * 上线集（用户可见 + 后端实际探测的服务）= [`SERVICE_IDS`] 去掉 [`PENDING_CALIBRATION_SERVICE_IDS`]。
 * 首页徽章只渲染本集合——停飞的服务不留「恒灰占位」这种装饰徽章。
 */
export const ENABLED_SERVICE_IDS: readonly ServiceId[] = SERVICE_IDS.filter(
  (id) => !PENDING_CALIBRATION_SERVICE_IDS.includes(id)
);

/** 当前代理出口（cdn-cgi/trace 打点得到，兼作缓存 key）。region = countryCode，可缺。 */
export interface UnlockEgress {
  ip: string;
  region?: string;
}

/**
 * 国家级网络封锁地区：出口位于其中时，海外服务 checker 的 timeout 是**结构性预期**（出口侧远端解析被 GFW 投毒
 * + 连接层 SNI/IP 拦截，用户真实流量同路径同死）——非低置信瞬态。据此按高置信终态收敛（正常 TTL 缓存、免 warm
 * 补测/settle-retry churn、G-flip2 不循环 invalidate），UI 显精确提示而非误判故障。checker 域名走 socks5 ATYP=domain
 * 透传出口远端解析、不经本机 DNS → 无「换 clean 解析器」可修（大陆视角无干净解析源）。最小集起步（本次故障面仅 CN）。
 */
export const RESTRICTED_EGRESS_REGIONS: ReadonlySet<string> = new Set(['CN']);
export const isRestrictedEgressRegion = (region?: string): boolean =>
  !!region && RESTRICTED_EGRESS_REGIONS.has(region.toUpperCase());

/**
 * gating 短路原因：
 * - `proxy-not-running`：代理核未运行。
 * - `exit-invalid`：选中 TS 出口 API 直判无效（未选出口设备 / 出口离线 / 未广告）——经无效出口检测必死出口空转，
 *   一律短路（零网络、零就绪门）。所有触发面汇聚 run() 单点收口，翻转对账（reconcileTsExitBlock）恢复后 invalidate 重检。
 */
export type UnlockBlockedReason = 'proxy-not-running' | 'exit-invalid';

/** 一次检测的完整快照（unlock:run / unlock:get 的返回体）。 */
export interface UnlockSnapshot {
  /** serviceId → 结果；gating 短路时为空对象。 */
  results: Record<string, UnlockResult>;
  /** 完成时间戳；null = 未检测（gating 短路）。 */
  checkedAt: number | null;
  /** 出口信息；trace 失败为 null。 */
  egress: UnlockEgress | null;
  /** 非空 = 未真正检测（如代理未运行）。 */
  blockedReason?: UnlockBlockedReason;
  /**
   * true = 核已 running 但 inbound 尚未就绪（就绪门 egress 探测重试耗尽 / 退避期间被 invalidate）→ 本轮未跑 checker、
   * checkedAt=null。**已提交为终态快照**（防 mount/切 tab 反复重扫死出口就绪门，由 run() 的 S-gate 兜住）；解除 =
   * invalidate（一切真状态变化：起停/切节点/G-flip/G-flip2 均清 lastSnapshot）或 force。区别于 blockedReason（gating 短路）。
   */
  notReady?: boolean;
  /**
   * true = 就绪门过了、但 6 checker 全 timeout（egress flap 期典型）。低置信瞬态：已提交 lastSnapshot（UI 如实 + mount
   * 水合零网络）但**未写 egressIp 缓存**——避免垃圾快照锁 30min；下一真触发（含 G-flip2 伴测成功）即重检。命中任一
   * marker 的高置信结果不置此标、照常入缓存。
   */
  lowConfidence?: boolean;
}

/** 单个 checker settle 时的增量推送（EVENT_UNLOCK_PROGRESS 载荷）。 */
export interface UnlockProgress {
  serviceId: string;
  result: UnlockResult;
}

/**
 * EVENT_UNLOCK_INVALIDATED 载荷（issue 2 review 修）：由**主进程**在 invalidate 时带上核真态，供渲染端决定
 * 「显示检测中(beginUnlockCheck) vs 复位 idle(resetUnlock)」——不再用渲染端可能陈旧的 connectionStatus/proxyBlocked
 * （invalidate 常先于 EVENT_PROXY_STARTED / IP_INFO_UPDATED 抵达，陈旧视图会误判分支）。
 */
export interface UnlockInvalidatedPayload {
  /** invalidate 时核是否在运行。 */
  running: boolean;
  /** invalidate 时选中出口是否直判无效（R-gate）：true 则不显检测中 spinner。 */
  exitBlocked: boolean;
}
