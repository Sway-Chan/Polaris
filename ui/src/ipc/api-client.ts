/**
 * Polaris api-client —— Polaris api-client 的 Tauri 2 移植。
 *
 * 设计纪律（迁移核心约束）：
 *  1. **方法签名 100% 对齐 Polaris**（方法名 / 参数 / 返回类型不变）——前端组件调用零改动，只换底层
 *     （Electron ipcRenderer → Tauri invoke/listen）。这样后续阶段补组件时 import 路径与方法全部沿用。
 *  2. 命令名 = Rust `#[tauri::command]` 函数名（snake_case，如 'proxy_start'、'config_get'），
 *     经 IPC_CHANNELS 引用。**注意**：Tauri 的命令名就是 Rust 函数名，冒号在 Rust 标识符里不合法，
 *     故 Electron 风格的 'proxy:start' 永远匹配不上（历史坑：曾按「Rust 注册名 = IPC_CHANNELS 串」
 *     设计，但 Tauri 不支持带冒号的命令名，运行期全部 `Command not found`）。命名规则见 ipc-channels.ts 头注释。
 *     event 名不受此限（自由字符串，保留冒号，对齐 src-tauri/src/events.rs）。
 *  3. **有 {success, data} 信封，且已由 ipc-client 拆掉**：Rust 侧所有 command 统一返回
 *     `ApiResponse<T>` = `{ success, data?, error?, code? }`（见 src-tauri/src/response.rs，95/95 零例外），
 *     与 Polaris Electron 期 registerIpcHandler 自封的信封逐字段一致。拆包点唯一——`ipc-client.invoke`：
 *     `success:true` → 返 data；`success:false` → throw IpcError（带 error/code）。
 *     故**本层方法签名一律标「解包后」的类型**（`Promise<UserConfig>` 而非 `Promise<ApiResponse<UserConfig>>`），
 *     后端业务失败以 IpcError 走 Promise reject，前端 catch 即得错误文案 + 结构化 code。
 *  4. 裸标量参数（Polaris 直接传 string/boolean 的通道，如 privacy:setPassword 的某些路径）经 invokeScalar
 *     包成 { value }——前端调用方仍传裸标量（签名兼容），仅底层转换。
 *
 * 覆盖范围：proxy / config / privacy / server / rules / logs / autoStart / connections / system /
 * ruleResources / ipInfo / unlock / version / update / coreUpdate / subscription / localImport /
 * backup / diagnostic / helper / app / window。与 Polaris api-client.ts 全方法对齐（933 行 → 同语义）。
 */

import { invoke, invokeScalar, listen, listenReady } from './ipc-client';
import { IPC_CHANNELS } from '../domain/ipc-channels';
import type {
  UserConfig,
  ServerConfig,
  ProxyStatus,
  ProxyErrorCode,
  LogEntry,
  RuntimeLogLevel,
  Rule,
  AutoStartStatus,
  SubscriptionConfig,
  HelperStatus,
  SystemProxyStatus,
  IpInfoSnapshot,
  SystemProcessInfo,
  RuleResourceDeleteResult,
  RuleResourceListItem,
  RuleResourceDownloadItem,
  RuleResourceDownloadResult,
  RuleResourceProgress,
  RuleResourceCatalogResult,
  InvalidNodeInfo,
  PendingNodeChanges,
  ProxyLifecycleEvent,
  StagedClassification,
  SaveOutcome,
  ImportParseResult,
} from '../contracts/types';
import type {
  UnlockSnapshot,
  UnlockProgress,
  UnlockInvalidatedPayload,
} from '../contracts/unlock-detection';
import type { SubscriptionPreviewResult } from '../contracts/subscription-preview';
import type { SubscriptionUpdateProgress } from '../contracts/subscription-progress';
import type { WarpWireGuardDraft } from '../domain/warp';
import type { BackupCategory } from '../domain/backup-categories';
import type {
  TailscaleStatusEvent,
  TailscaleStatusSnapshot,
} from '../contracts/tailscale-status';
import type { TaildropInbox, TaildropSaveResult } from '../contracts/taildrop';
import type { SpeedTestDonePayload, SpeedTestInvokeResult } from '../contracts/speed-test';
import type { CoreBuildKind } from '../domain/core-build';

// ============================================================================
// proxyApi
// ============================================================================

export const proxyApi = {
  /**
   * 启动内核。**无参 —— 起核用哪份配置由后端读盘决定**（因果全在 Rust `proxy_start` 头注）。
   *
   * 曾经收 `config: UserConfig` 并把渲染端的 `app-store.config` 传进去。那份内存副本只靠
   * `event:configChanged` → `loadConfig(true)` 异步刷新，于是「写盘 → 立刻点启动」会用**写之前**
   * 的配置起核。载荷还是有损的（`config_get` strip 了隐私密码哈希）。删参数比「让调用方记得先刷」
   * 可靠：调用方无从知道回声到没到。
   */
  async start(): Promise<void> {
    return invoke(IPC_CHANNELS.PROXY_START);
  },

  async stop(): Promise<void> {
    return invoke(IPC_CHANNELS.PROXY_STOP);
  },

  async restart(): Promise<void> {
    // 无参：后端自己读盘（见 Rust `proxy_restart` 头注）。此前这里先 `config:get` 一趟再把结果传回去，
    // 两次 IPC 之间有一个能被别人写盘挤进去的窗口，且平白多一次往返。
    return invoke(IPC_CHANNELS.PROXY_RESTART);
  },

  async getStatus(): Promise<ProxyStatus> {
    return invoke(IPC_CHANNELS.PROXY_GET_STATUS);
  },

  /**
   * 自定义协议兼容性 probe：当前内核能否识别该 outbound（sing-box check）。
   *
   * `error` 不再是 `sing-box check` stderr 的前 300 字符截断（旧行为，零结构化）——现在是后端
   * `parse_probe_diagnostic` 解析出的人类可读消息；`errorPath` 是配套解出的键路径（解析不出 → 该键
   * 整个不下发，不是空串，调用方须用 `?.`/`in` 判断，不能拿空串当「无路径」）；`errorRaw` 是完整原始
   * 输出（ANSI 已剥离），供兜底展示。三个新字段只在 `ok:false` 且非 `indeterminate` 时有意义。
   */
  async probeOutbound(
    outbound: unknown,
    isEndpoint?: boolean
  ): Promise<{
    ok: boolean;
    indeterminate?: boolean;
    error?: string;
    errorPath?: string;
    errorRaw?: string;
  }> {
    return invoke(IPC_CHANNELS.KERNEL_PROBE_OUTBOUND, { outbound, isEndpoint });
  },

  /** 用户主动清理系统代理残留设置（TUN 残留提示的一键恢复动作）。 */
  async disableSystemProxy(): Promise<{ ok: boolean }> {
    return invoke(IPC_CHANNELS.SYSTEM_PROXY_DISABLE);
  },

  /**
   * 系统代理**活态**查询：当前 OS 代理是否仍指向本进程的 mixed 入站（读 `pointsToUs`，
   * 别自己拿 `enabled` 判 —— 契约见 `SystemProxyStatus`）。
   *
   * 后端每次调用会 exec `networksetup`/`gsettings`/`reg`（mac 三次），**属有成本的查询**：
   * 调用方须低频、且只在 systemProxy 接管 + 核在跑 + 窗口可见时取（见 StatusBar 的 `useSystemProxyLive`）。
   * 核未运行 / 读取受阻 → reject（**不返回 false**）：读不到 ≠ 没生效，调用方应折成「未知」而非「未生效」。
   */
  async getSystemProxyStatus(): Promise<SystemProxyStatus> {
    return invoke(IPC_CHANNELS.SYSTEM_PROXY_GET_STATUS);
  },

  /** §2 待应用差集（pull）：节点集相对运行核**起核快照**的增/改/删。核未运行 → 三个集合全空。 */
  async getPendingChanges(): Promise<PendingNodeChanges> {
    return invoke(IPC_CHANNELS.PROXY_GET_PENDING_CHANGES);
  },

  /** §2 动作条「立即应用」：把最新 config force-restart 入核。 */
  async applyPendingChanges(): Promise<{
    ok: boolean;
    status: 'applied' | 'deferred' | 'skipped';
  }> {
    return invoke(IPC_CHANNELS.PROXY_APPLY_PENDING_CHANGES);
  },

  /**
   * 代理已启动。**payload 恒为空对象**——后端 `commands/proxy.rs:41,76` emit `json!({})`。
   *
   * 此前这里声明了 `pid`/`startTime`/`autoRestarted` 三个字段，但后端从来不发 → 恒 undefined，
   * 属「契约声明了、后端没接」那一类死契约（本轮审计在 33 个事件常量里找到 16 条同类死通道）。
   * 删声明而非补后端：连接态的权威源是 `proxy:getStatus`（含 startTime/pid），事件只作**变更信号**，
   * 订阅方收到即重拉真值即可（见 App.tsx 的全局订阅层），无需 payload 复制一份易过期的快照。
   */
  onStarted(listener: () => void): () => void {
    return listen(IPC_CHANNELS.EVENT_PROXY_STARTED, listener);
  },

  /** 代理已停止。payload 恒为空对象（同 [`onStarted`]：事件是信号，真值走 getStatus）。 */
  onStopped(listener: () => void): () => void {
    return listen(IPC_CHANNELS.EVENT_PROXY_STOPPED, listener);
  },

  /** 主进程各 emit 点 payload 形状不一，message 优先 / error 兜底。 */
  onError(
    listener: (data: {
      message?: string;
      error?: string;
      errorCode?: ProxyErrorCode;
      code?: number;
      signal?: string | null;
    }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_PROXY_ERROR, listener);
  },

  onAutoNodeSwitched(
    listener: (data: { reason: string; newServerName: string; latency: number }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_AUTO_NODE_SWITCHED, listener);
  },

  /**
   * R2 待应用差集 PUSH：后端 `switch_mode` 末尾 emit `event:proxyPendingChanges`。
   *
   * 载荷类型**必须**是 [`PendingNodeChanges`] 本身，不能就地写一个结构型 —— 后端 pull/push
   * 返回的是同一个 `PendingChangesSummary`，前端这边再分裂出第二份形状，契约就又有了两个真值源
   * （`modified` 恒空那次退化正是这么长出来的）。
   */
  onPendingChanges(listener: (data: PendingNodeChanges) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_PROXY_PENDING_CHANGES, listener);
  },

  /**
   * runtime 生命周期结局 PUSH：后端在**真状态跃迁点**发（`start_inner` 就绪 / `stop_inner` 拆除 /
   * `start` 包装的 Err 腿），覆盖 [`onStarted`]/[`onStopped`] 盖不住的全部后端自驱路径。
   *
   * 载荷只带「结局」这一位，**不带 pid / startTime** —— 那两个的权威源仍是 `proxy:getStatus`
   * （同 [`onStarted`] 头注那条既定结论：事件是变更信号，payload 不复制易过期的快照）。
   * 订阅方收到即重拉真值。
   */
  onLifecycle(listener: (data: ProxyLifecycleEvent) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_PROXY_LIFECYCLE, listener);
  },

  /** 监听启动前配置校验 gate 剔除的非法节点（空数组=本次启动无非法节点/清陈旧标灰）。 */
  onInvalidNodes(listener: (data: InvalidNodeInfo[]) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_PROXY_INVALID_NODES, listener);
  },

  /** 监听 Tailscale 交互登录 URL。 */
  onTailscaleAuth(
    listener: (data: {
      nodeName: string;
      url: string;
      transient?: boolean;
      serverId?: string;
    }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_TAILSCALE_AUTH_URL, listener);
  },

  /** 监听 sing-box 1.14 管理 API 推送的 Tailscale 节点真实态。 */
  onTailscaleStatus(listener: (data: TailscaleStatusEvent) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_TAILSCALE_STATUS, listener);
  },

  /** 监听「启动前属主归一删掉某节点 root 残留 state」（登录态已失效）。 */
  onTailscaleStateCleared(listener: (data: { serverId: string }) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_TAILSCALE_STATE_CLEARED, listener);
  },

  /** 监听「登录期出口让位」事件。 */
  onMeshLoginFallback(
    listener: (data: { engaged: boolean; serverName?: string }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_MESH_LOGIN_FALLBACK, listener);
  },

  /** 监听 TUN 启动后的「无 marker 系统代理残留」提示。 */
  onSystemProxyResidual(listener: (data: { proxy: string }) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_SYSTEM_PROXY_RESIDUAL, listener);
  },

  /** #40：非官方核 ≤ 随包基线 → 兼容风险提醒。 */
  onCoreBaselineWarning(
    listener: (data: { current: string; bundled: string; kind: string }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_CORE_BASELINE_WARNING, listener);
  },
};

// ============================================================================
// configApi
// ============================================================================

export const configApi = {
  async get(): Promise<UserConfig> {
    return invoke(IPC_CHANNELS.CONFIG_GET);
  },

  /**
   * 全量保存 config。
   *
   * `deferRestart`（可选）= 暂存层「保存」腿的**不主动重启**标志：结构性变更只落盘 + 进待应用差集，
   * 断流时机由用户点「立即应用」决定。**不传 = 今天行为**（落盘即去抖重启），既有十余个调用点零改动。
   * 射程只到 switch-engine 第 4 腿；`must_restart`（不重启就静默不生效）那类**不会**被降级。
   */
  async save(
    config: UserConfig,
    deferRestart?: boolean,
    baseVersion?: string
  ): Promise<SaveOutcome> {
    // Rust config_save(config, defer_restart: Option<bool>, base_version: Option<String>)
    // —— 参数袋 key = `config` / `deferRestart` / `baseVersion`。
    // 不传时**不放键**（而非放 `undefined`）：Tauri 的参数袋对 `Option<_>` 只认「键缺席」，
    // 显式 undefined 在 serde 侧同样落 None，但留个 undefined 键会让 check:ipc 的键集比对多一项噪音。
    // 参数袋必须是**对象字面量**（不是先攒一个 `args` 变量再传）—— `check-ipc-args.mjs` 静态取键，
    // 裸标识符它证明不了「覆盖了 required 参数」，直接判红。故这里逐分支写死。
    if (baseVersion !== undefined) {
      // `baseVersion` 缺省 = 关掉乐观并发校验 = 今天行为（既有十余个调用点零改动）。
      // 走校验的只有暂存层「保存」腿，它恒同时传 `deferRestart`；即便不传，Tauri 侧
      // `Option<bool>` 收到 undefined 与键缺席同义（`unwrap_or(false)`）。
      return invoke(IPC_CHANNELS.CONFIG_SAVE, { config, deferRestart, baseVersion });
    }
    return deferRestart === undefined
      ? invoke(IPC_CHANNELS.CONFIG_SAVE, { config })
      : invoke(IPC_CHANNELS.CONFIG_SAVE, { config, deferRestart });
  },

  /**
   * 预告：这份候选配置若现在落盘会走哪条腿（只读，不落盘、不碰核）。
   *
   * 用于暂存条目在**保存之前**标注「保存即生效 / 需重启生效」——「5 项待保存 → 保存 → 2 项待应用」
   * 这个收缩必须在暂存期就有交代，否则用户会认为保存吃掉了另外 3 条。
   */
  async classifyStaged(config: UserConfig): Promise<StagedClassification> {
    return invoke(IPC_CHANNELS.CONFIG_CLASSIFY_STAGED, { config });
  },

  async updateMode(mode: UserConfig['proxyMode']): Promise<void> {
    return invoke(IPC_CHANNELS.CONFIG_UPDATE_MODE, { mode });
  },

  async getValue<T = unknown>(key: string): Promise<T> {
    return invoke(IPC_CHANNELS.CONFIG_GET_VALUE, { key });
  },

  async setValue(key: string, value: unknown): Promise<void> {
    return invoke(IPC_CHANNELS.CONFIG_SET_VALUE, { key, value });
  },

  /**
   * 配置变更的**无载荷信号**：后端 emit `{}`（`commands/config.rs` 的
   * `broadcast_config_changed_with`），收到即各自重拉，没有任何消费方读 payload。
   *
   * 签名收成零参不是文档性质的：它让「想读 newValue」在类型层就编不过。此前那份
   * `{ key?, oldValue?, newValue? }` 是照搬 Electron 侧的形状，而 `newValue` 经脱敏、
   * 也没走 `config_get` 那侧的 bypassLANList 补齐 —— 直接拿来用是错的（见 `useConfig.ts`）。
   */
  onChanged(listener: () => void): () => void {
    return listen(IPC_CHANNELS.EVENT_CONFIG_CHANGED, listener);
  },

  async getPrivacyMode(): Promise<boolean> {
    return invoke(IPC_CHANNELS.CONFIG_GET_PRIVACY_MODE);
  },

  async setPrivacyMode(value: boolean): Promise<void> {
    // Polaris 原直接传裸 boolean；Tauri 需对象，底层包 { value }。
    return invokeScalar(IPC_CHANNELS.CONFIG_SET_PRIVACY_MODE, value);
  },

  /**
   * 进入隐私模式（锁屏）。后端 `config_set_privacy_mode(true)` 状态跃迁时真 emit
   * （config.rs:355-362：仅 prev≠value 才发）；托盘「立即锁定」/ idle 计时 / 别的窗口均经此收敛主窗遮罩。
   */
  onEnterPrivacyMode(listener: () => void): () => void {
    return listen(IPC_CHANNELS.EVENT_ENTER_PRIVACY_MODE, listener);
  },

  /** 退出隐私模式。后端 `config_set_privacy_mode(false)` 真 emit（解锁成功后由前端调 setPrivacyMode(false) 触发）。 */
  onExitPrivacyMode(listener: () => void): () => void {
    return listen(IPC_CHANNELS.EVENT_EXIT_PRIVACY_MODE, listener);
  },
};

// ============================================================================
// privacyApi —— F29：隐私密码。哈希/校验全在后端；渲染端只拿 hasPassword 布尔与 verify 结果。
// ============================================================================

export const privacyApi = {
  // Rust privacy_set_password(_password: String) / privacy_unlock(_password: String) —— 参数袋 key = `password`
  // （**非** `plain`）。裸 { plain } 缺 required key `password` → missing-key 崩。
  setPassword: (plain: string): Promise<{ success: boolean }> =>
    invoke(IPC_CHANNELS.PRIVACY_SET_PASSWORD, { password: plain }),
  unlock: (plain: string): Promise<{ ok: boolean }> =>
    invoke(IPC_CHANNELS.PRIVACY_UNLOCK, { password: plain }),
  hasPassword: (): Promise<boolean> => invoke(IPC_CHANNELS.PRIVACY_HAS_PASSWORD),
};

// ============================================================================
// serverApi
// ============================================================================

export const serverApi = {
  async getAll(): Promise<ServerConfig[]> {
    return invoke(IPC_CHANNELS.SERVER_GET_ALL);
  },

  /**
   * 新增节点。**返回 void，不回传新建节点** —— 后端 `server_add`（`commands/server.rs:69`）返
   * `ApiResponse<()>`。
   *
   * 此前这里声明 `Promise<ServerConfig>` 是**类型谎言**：运行期拿到的恒是 undefined，任何
   * 「add 完取回 id」的写法都会静默拿到 undefined（TsLoginDialog 就踩过，改走渲染端自带 id）。
   * 要新建节点的 id → 渲染端自己 mint（`crypto.randomUUID()`）后放进 server：后端 `ensure_server_id`
   * 只在 id 缺失/空串时才 mint，**非空 id 原样保留**（其单测名即 `..._keeps_existing`）。
   */
  async add(server: Omit<ServerConfig, 'id'> | ServerConfig): Promise<void> {
    // Rust server_add(server: Value) —— 参数袋 key = `server`。
    return invoke(IPC_CHANNELS.SERVER_ADD, { server });
  },

  /** 批量添加自建节点（本地导入，一次写盘） */
  async addBulk(servers: ServerConfig[]): Promise<{ added: number }> {
    return invoke(IPC_CHANNELS.SERVER_ADD_BULK, { servers });
  },

  async update(server: ServerConfig): Promise<void> {
    // Rust server_update(server: Value) —— 参数袋 key = `server`。
    return invoke(IPC_CHANNELS.SERVER_UPDATE, { server });
  },

  /**
   * 删除服务器。fallbackSelectedId：删的是当前选中节点时的兜底出口（最快剩余节点）；
   * 后端据此把 selectedServerId 置兜底节点并 emit 触发重启（D4）。
   */
  async delete(serverId: string, fallbackSelectedId?: string | null): Promise<void> {
    return invoke(IPC_CHANNELS.SERVER_DELETE, { serverId, fallbackSelectedId });
  },

  /** 批量删除服务器（一次配置写，避免并发单删竞态）。返回实际删除数。 */
  async deleteBatch(
    serverIds: string[],
    fallbackSelectedId?: string | null
  ): Promise<number> {
    return invoke(IPC_CHANNELS.SERVER_DELETE_BATCH, {
      serverIds,
      fallbackSelectedId,
    });
  },

  /** Phase 2 按需登录：拉起瞬态登录核取交互登录 URL。 */
  async tailscaleLogin(server: ServerConfig): Promise<{
    started: boolean;
    reason?: 'alreadyLoggedIn' | 'inMainCore' | 'alreadyRunning';
    authUrl?: string;
  }> {
    return invoke(IPC_CHANNELS.TAILSCALE_LOGIN, { server });
  },

  /** 取消某节点在飞的瞬态登录核（用户手动取消）。 */
  async tailscaleLoginCancel(serverId: string): Promise<void> {
    return invoke(IPC_CHANNELS.TAILSCALE_LOGIN_CANCEL, { serverId });
  },

  /** 退出登录：清该节点 Tailscale 持久登录会话（state 目录）。 */
  async tailscaleLogout(serverId: string): Promise<{ runningNeedsRestart: boolean }> {
    return invoke(IPC_CHANNELS.TAILSCALE_LOGOUT, { serverId });
  },

  /** 批量查 TS 节点 state 目录存在性（不起核判「登录过没」）。 */
  async tailscaleStateExists(serverIds: string[]): Promise<Record<string, boolean>> {
    return invoke(IPC_CHANNELS.TAILSCALE_STATE_EXISTS, { serverIds });
  },

  /** L2：主动拉各 TS 节点状态末帧(self IP/peers) + 新鲜度(connected)。 */
  async tailscaleGetStatus(): Promise<TailscaleStatusSnapshot> {
    return invoke(IPC_CHANNELS.TAILSCALE_GET_STATUS);
  },

  /** 读一次该 TS 节点的 Taildrop 收件箱（首帧快照）。失败抛 `IpcError`，`code` 见 `domain/taildrop.ts`。 */
  async taildropList(serverId: string): Promise<TaildropInbox> {
    return invoke(IPC_CHANNELS.TAILDROP_LIST, { serverId });
  },

  /** 清未读角标。**不删文件** —— 待处理数不变。 */
  async taildropMarkRead(serverId: string): Promise<void> {
    return invoke(IPC_CHANNELS.TAILDROP_MARK_READ, { serverId });
  },

  /** 删除收件箱里的一个文件。 */
  async taildropDelete(serverId: string, name: string): Promise<void> {
    return invoke(IPC_CHANNELS.TAILDROP_DELETE, { serverId, name });
  },

  /** 取消一个接收中的文件。`senderId` + `name` 必须成对（同名文件可来自不同发件人）。 */
  async taildropCancel(serverId: string, senderId: string, name: string): Promise<void> {
    return invoke(IPC_CHANNELS.TAILDROP_CANCEL, { serverId, senderId, name });
  },

  /** 取件：后端开原生保存框再写盘。`canceled:true` = 用户取消，**不是失败**。 */
  async taildropSave(serverId: string, name: string): Promise<TaildropSaveResult> {
    return invoke(IPC_CHANNELS.TAILDROP_SAVE, { serverId, name });
  },

  async switch(serverId: string): Promise<void> {
    return invoke(IPC_CHANNELS.SERVER_SWITCH, { serverId });
  },

  async generateUrl(server: ServerConfig): Promise<string> {
    return invoke(IPC_CHANNELS.SERVER_GENERATE_URL, { server });
  },

  /** Cloudflare WARP：注册匿名设备 → 返回 WireGuard 草稿。 */
  async registerWarp(licenseKey?: string): Promise<WarpWireGuardDraft> {
    return invoke(IPC_CHANNELS.WARP_REGISTER, { licenseKey });
  },

  /** 对已注册 WARP 节点原地应用 WARP+ license（升级免重建）。 */
  async applyWarpLicense(
    serverId: string,
    license: string
  ): Promise<{ ok: boolean; warpPlus?: boolean; error?: string }> {
    return invoke(IPC_CHANNELS.WARP_APPLY_LICENSE, { serverId, license });
  },

  /** 测试指定服务器延迟，不传则测试所有服务器。 */
  async speedTest(serverIds?: string[]): Promise<SpeedTestInvokeResult> {
    return invoke(IPC_CHANNELS.SERVER_SPEED_TEST, { serverIds });
  },

  /** 订阅测速单个节点完成事件（流式增量显示，不等队列）。 */
  onSpeedTestResult(
    listener: (data: { serverId: string; latency: number }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_SPEED_TEST_RESULT, listener);
  },

  /** 订阅测速进度事件（已测/成功/总数）。 */
  onSpeedTestProgress(
    listener: (data: { tested: number; ok: number; total: number }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_SPEED_TEST_PROGRESS, listener);
  },

  /**
   * 订阅一轮测速的**终态**（`{outcome,tested,total,serverIds,pending}`）。
   *
   * 广播通道 ⇒ **不管是谁发起的**（主窗 / 托盘浮层）都收得到。进度 toast 的终态判定以它为主路径，
   * 静默超时降级为纯兜底。载荷语义见 `contracts/speed-test.ts` 的 `SpeedTestDonePayload`。
   */
  onSpeedTestDone(listener: (data: SpeedTestDonePayload) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_SPEED_TEST_DONE, listener);
  },
};

// ============================================================================
// rulesApi
// ============================================================================

export const rulesApi = {
  async getAll(): Promise<Rule[]> {
    return invoke(IPC_CHANNELS.RULES_GET_ALL);
  },

  async add(rule: Omit<Rule, 'id'>): Promise<Rule> {
    // Rust rules_add(rule: Value) —— 参数袋 key = `rule`。
    return invoke(IPC_CHANNELS.RULES_ADD, { rule });
  },

  async update(rule: Rule): Promise<void> {
    // Rust rules_update(rule: Value) —— 参数袋 key = `rule`。
    return invoke(IPC_CHANNELS.RULES_UPDATE, { rule });
  },

  async delete(ruleId: string): Promise<void> {
    return invoke(IPC_CHANNELS.RULES_DELETE, { ruleId });
  },

  /** 重排规则：orderedIds 为全部规则 id 的新顺序 */
  async reorder(orderedIds: string[]): Promise<void> {
    return invoke(IPC_CHANNELS.RULES_REORDER, { orderedIds });
  },
};

// ============================================================================
// logsApi
// ============================================================================

export const logsApi = {
  async get(subscriptionId: string, limit?: number): Promise<LogEntry[]> {
    return invoke(IPC_CHANNELS.LOGS_GET, { subscriptionId, limit });
  },

  /** 在后端保留历史中检索；返回数量受绘制预算限制，但查询域不受前端 500 行尾部限制。 */
  async search(
    query: string,
    level: LogEntry['level'],
    source: 'all' | 'sing-box' | 'app',
    limit?: number,
  ): Promise<LogEntry[]> {
    return invoke(IPC_CHANNELS.LOGS_SEARCH, { query, level, source, limit });
  },

  async unsubscribe(subscriptionId: string): Promise<void> {
    return invoke(IPC_CHANNELS.LOGS_UNSUBSCRIBE, { subscriptionId });
  },

  async clear(): Promise<void> {
    return invoke(IPC_CHANNELS.LOGS_CLEAR);
  },

  /** 导出纯日志（节点身份打码；不含配置与运行态，区别于 diagnostic.export 的完整诊断报告）。 */
  async export(): Promise<{ success: boolean; filePath?: string; error?: string }> {
    return invoke(IPC_CHANNELS.LOGS_EXPORT);
  },

  /**
   * 在系统文件管理器里打开日志目录（G3，原型 log 工具栏「目录」）。
   *
   * 打开的是**配置目录**——应用日志在 `logs/polaris.log`、内核日志在 `singbox.log`，
   * 两者不同层，开二者的共同父目录才不会让最常要的 singbox.log 落在视野外（见 Rust 侧命令注释）。
   * 路径解析在后端，前端不拼路径（三平台不同，portable 形态另有落点）。
   */
  async openDir(): Promise<void> {
    return invoke(IPC_CHANNELS.LOGS_OPEN_DIR);
  },

  /**
   * 读回核**此刻实际**在用的日志级别（管理 API `GetDefaultLogLevel`）。
   *
   * 与 `config.logLevel` 不是同一件事 —— 后者是「我写下的意图」，两者已知有两条分叉：隐私锁开启时
   * 生成侧把级别抬到 ≥warn；配置暂存态下改级别零落盘。**读不到时后端回 `level: null` 而不是某个
   * 具体级别**（回落出来的一定是那个「我写下的值」，自证就退化成它本要揭穿的那句谎）。
   */
  async runtimeLevel(): Promise<RuntimeLogLevel> {
    return invoke(IPC_CHANNELS.LOGS_RUNTIME_LEVEL);
  },

  /** 当前进程是否临时启用了 DEBUG 诊断；不读取/修改持久配置。 */
  async diagnosticState(): Promise<boolean> {
    return invoke(IPC_CHANNELS.LOGS_DIAGNOSTIC_STATE);
  },

  /** 临时抬高本次运行的日志门槛；应用重启后由启动配置自然恢复。 */
  async setDiagnostic(enabled: boolean): Promise<boolean> {
    return invoke(IPC_CHANNELS.LOGS_SET_DIAGNOSTIC, { enabled });
  },

  /** 等待批量日志监听真正登记完成；水合必须在此之后启动，才能保证快照与直播无缝。 */
  onReceivedBatchReady(listener: (logs: LogEntry[]) => void): Promise<() => void> {
    return listenReady(IPC_CHANNELS.EVENT_LOG_RECEIVED_BATCH, listener);
  },
};

// ============================================================================
// autoStartApi
// ============================================================================

export const autoStartApi = {
  async set(enabled: boolean): Promise<boolean> {
    return invoke(IPC_CHANNELS.AUTO_START_SET, { enabled });
  },

  async getStatus(): Promise<AutoStartStatus> {
    return invoke(IPC_CHANNELS.AUTO_START_GET_STATUS);
  },
};

// ============================================================================
// statsApi —— batch3 §3.7：订阅驱动数据面。
// 渲染端按 topic 声明订阅，后端据订阅集派生 worker demand + 精确 relay。
// ============================================================================

import type { StatsTopic } from '../domain/ipc-channels';
import { STATS_TOPIC_EVENT } from '../domain/ipc-channels';
import type { TrafficStats } from '../contracts/types';
import type {
  ConnectionsDetailUpdate,
  ConnectionsAggregate,
  ConnectionsClosedSnapshot,
  ConnectionsClosedUpdate,
} from '../contracts/types';

export const statsApi = {
  /** 订阅某 topic（stats|aggregate|detail|closed）：后端挂订阅 + 即回初始帧。 */
  async subscribe(topic: StatsTopic): Promise<void> {
    return invoke(IPC_CHANNELS.STATS_SUBSCRIBE, { topic });
  },

  /** 退订某 topic（unmount/窗口隐藏/暂停）：无订阅者 → worker 逐级停机。 */
  async unsubscribe(topic: StatsTopic): Promise<void> {
    return invoke(IPC_CHANNELS.STATS_UNSUBSCRIBE, { topic });
  },

  /** 在完整活动连接表上先过滤，再按首页画布槽位投影；空 query 即常态流向。 */
  async projectTopology(query: string, slots: number): Promise<ConnectionsAggregate> {
    return invoke(IPC_CHANNELS.STATS_PROJECT_TOPOLOGY, { query, slots });
  },

  /** stats topic：流量统计推送。 */
  onStatsUpdated(listener: (data: TrafficStats) => void): () => void {
    return listen<TrafficStats>(STATS_TOPIC_EVENT.stats, listener);
  },

  /** aggregate topic：连接导航的有界目标/出口排名。 */
  onConnectionsAggregate(
    listener: (data: ConnectionsAggregate) => void
  ): () => void {
    return listen<ConnectionsAggregate>(STATS_TOPIC_EVENT.aggregate, listener);
  },

  /** 完整活动表流向字段变化；常态/检索投影共用该信号。 */
  onConnectionsTopologyChangedReady(listener: () => void): Promise<() => void> {
    return listenReady<number>(IPC_CHANNELS.EVENT_CONNECTIONS_TOPOLOGY_CHANGED, listener);
  },

  /** detail topic：活动连接 reset 基线 + 常态增量。 */
  onConnectionsDetail(
    listener: (data: ConnectionsDetailUpdate) => void
  ): () => void {
    return listen<ConnectionsDetailUpdate>(STATS_TOPIC_EVENT.detail, listener);
  },

  /** closed topic：独立的已结束连接历史。 */
  onConnectionsClosed(
    listener: (data: ConnectionsClosedUpdate) => void
  ): () => void {
    return listen<ConnectionsClosedUpdate>(STATS_TOPIC_EVENT.closed, listener);
  },

  /** 清空已结束历史并设置重放水位。 */
  async clearClosed(): Promise<ConnectionsClosedSnapshot> {
    return invoke(IPC_CHANNELS.STATS_CLOSED_CLEAR);
  },
};

// ============================================================================
// connectionsApi —— §3.7：明细/聚合改订阅驱动；此处仅留关连接的命令式动作。
// ============================================================================

export const connectionsApi = {
  /** 关单条连接（后端经 9090 DELETE /connections/{id}）。 */
  async close(id: string): Promise<{ ok: boolean }> {
    return invoke(IPC_CHANNELS.CONNECTIONS_CLOSE, { id });
  },
  /** 关全部连接（后端经 9090 DELETE /connections，触发 ResetNetwork）。 */
  async closeAll(): Promise<{ ok: boolean }> {
    return invoke(IPC_CHANNELS.CONNECTIONS_CLOSE_ALL);
  },
};

// ============================================================================
// systemApi
// ============================================================================

export const systemApi = {
  /** 枚举当前系统进程（聚合去重，供进程规则快速选择）。 */
  async listProcesses(): Promise<SystemProcessInfo[]> {
    return invoke(IPC_CHANNELS.SYSTEM_LIST_PROCESSES);
  },
  /** 用系统默认浏览器打开外部链接。 */
  async openExternal(url: string): Promise<void> {
    // Rust shell_open_external(url: String) —— 参数袋 key = `url`（裸标量漏包会 missing key）。
    return invoke(IPC_CHANNELS.SHELL_OPEN_EXTERNAL, { url });
  },
};

// ============================================================================
// windowApi —— 窗口 chrome 控制（Win/Linux 自绘 titlebar min/max/close，`decorations:false` 下唯一入口）
// ============================================================================

export const windowApi = {
  async minimize(): Promise<void> {
    return invoke(IPC_CHANNELS.WINDOW_MINIMIZE);
  },

  async maximizeToggle(): Promise<void> {
    return invoke(IPC_CHANNELS.WINDOW_MAXIMIZE_TOGGLE);
  },

  async close(): Promise<void> {
    return invoke(IPC_CHANNELS.WINDOW_CLOSE);
  },

  async isMaximized(): Promise<boolean> {
    return invoke(IPC_CHANNELS.WINDOW_IS_MAXIMIZED);
  },

  /** 最大化态变更（含按钮外触发：WM 双击标题栏 / 拖顶等），标题栏图标据此跟随。 */
  onMaximizeChanged(listener: (data: { maximized: boolean }) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_WINDOW_MAXIMIZE_CHANGED, listener);
  },

  /**
   * 重启 Polaris 本体（U-7 第三类重启）。
   *
   * **会停核**：后端走 `request_restart()` → `ExitRequested` → `run_exit_cleanup`（停 sing-box +
   * 清系统代理），故调用即等于「断开代理并重启」。调用点必须已向用户交代过这一点。
   *
   * 后端只是往事件循环投一条退出请求就立即返回，真正的停核+重启在其后异步发生 ⇒
   * **resolve 不代表重启成功**（进程可能在应答送达前就走完退出腿，那时 Promise 干脆不 settle）。
   * 只能用 reject 判「IPC 都没打通」，不要在 then 里接任何后续 UI 动作。
   */
  async restartApp(): Promise<void> {
    return invoke(IPC_CHANNELS.APP_RESTART);
  },

  /**
   * U-7 判据基线：本次进程**启动时**后端真正读到的那三个键的生效值（`UserConfig` 口径的「是否开」）。
   *
   * 只读、进程生命周期内不变。**不能**在渲染端自行快照代替：webview 自愈重载会让渲染端的
   * 「启动值」漂移到重载那一刻的磁盘值，而后端这份仍是真正的进程启动值。
   */
  async startupConfigFlags(): Promise<{
    hardwareAcceleration: boolean;
    windowEffects: boolean;
    rememberWindowSize: boolean;
  }> {
    return invoke(IPC_CHANNELS.APP_STARTUP_CONFIG_FLAGS);
  },

  /**
   * spec §2.5 Q1-b 清除时机 ④：上次进程是不是**正常退出**的？—— **读即清**。
   *
   * 真 ⇒ 上次走完了退出腿（托盘「退出」/ ⌘Q / 末窗关闭 / `app:restart`）；
   * 假 ⇒ 强杀 / 崩溃 / 断电，**或者进程压根没退**（webview 自愈重载、C16 轻量模式销毁重建）。
   * 这个区分只有主进程知道 —— 渲染端能拿到的 `beforeunload`/`pagehide` 在重载时同样触发。
   *
   * **每个进程只有第一次调用返回真**（后端在读的同一次系统调用里消费掉标记）；调用方据此
   * 决定「清不清持久化的暂存」，必须在恢复（hydrate）之前拿到。
   */
  async takeCleanExitFlag(): Promise<boolean> {
    return invoke(IPC_CHANNELS.APP_TAKE_CLEAN_EXIT_FLAG);
  },
};

// ============================================================================
// ruleResourcesApi —— .srs 下载/管理
// ============================================================================

export const ruleResourcesApi = {
  list(): Promise<RuleResourceListItem[]> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_LIST);
  },
  download(items: RuleResourceDownloadItem[]): Promise<RuleResourceDownloadResult[]> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_DOWNLOAD, { items });
  },
  redownload(id: string): Promise<RuleResourceDownloadResult> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_REDOWNLOAD, { id });
  },
  /**
   * 中止该资源的在途下载（原型 `res-cancel`）。返回 `cancelled` = **真被中止**的在途下载条数——
   * 0 表示点下去时已无可取消的下载（后端如实回报，不伪装成功）。取消的下载不落盘不入册。
   */
  cancel(id: string): Promise<{ cancelled: number }> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_CANCEL, { id });
  },
  delete(id: string, force?: boolean): Promise<RuleResourceDeleteResult> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_DELETE, { id, force });
  },
  getCatalog(): Promise<RuleResourceCatalogResult> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_GET_CATALOG);
  },
  refreshCatalog(): Promise<RuleResourceCatalogResult> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_REFRESH_CATALOG);
  },
  /**
   * 回读上次刷新落盘的全量清单（**零出站**）；从没刷新成功过 → `null`。
   * `null` 与 `refreshCatalog()` 的 `source:'builtin'` 不可混用：后者意味着「远程拉过且失败了」，
   * 而本调用一次网都没打，报成失败即谎报。
   */
  getCachedCatalog(): Promise<RuleResourceCatalogResult | null> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_GET_CACHED_CATALOG);
  },
  setAutoUpdate(args: {
    enabled: boolean;
    intervalHours?: number;
  }): Promise<{ ok: boolean }> {
    // Rust rule_resources_set_auto_update(_enabled: bool, _interval_hours: Option<u32>) —— 两个具名参数。
    // args 恰好即参数袋（键 enabled/intervalHours），但显式展开成对象字面量，
    // 让「参数袋形状」在调用点可被防回归门静态核对（禁裸标识符透传）。
    return invoke(IPC_CHANNELS.RULE_RESOURCES_SET_AUTO_UPDATE, {
      enabled: args.enabled,
      intervalHours: args.intervalHours,
    });
  },
  updateAll(): Promise<RuleResourceDownloadResult[]> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_UPDATE_ALL);
  },
  resetBuiltin(tag: string): Promise<RuleResourceDownloadResult> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_RESET_BUILTIN, { tag });
  },
  /**
   * 更新单个内置 geo 规则集到上游最新版。`tag` 是内置表里的 tag（如 `geosite-cn`），**不是** `builtin:` id。
   * 内置项不入 `config.ruleResources`，故不能走 `redownload`（那条按 id 查册，对内置恒 NOT_FOUND）。
   * 只换 `<userData>/rules/` 里的文件，不重启内核 —— 生效要等下次起核。
   */
  updateBuiltin(tag: string): Promise<RuleResourceDownloadResult> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_UPDATE_BUILTIN, { tag });
  },
  /** 图标库拉取（经后端统一会话）。全失败返 []，UI 回落手动输入图标 URL。 */
  fetchIconGalleries(): Promise<Array<{ name: string; url: string }>> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_ICON_GALLERIES);
  },
  /**
   * 强制刷新图标库：后端把清单内存缓存（1h TTL）与图标本体的磁盘浏览缓存
   * （`<userData>/icons/remote/`）**两层一起**作废后重拉，返回新清单。返回契约同
   * `fetchIconGalleries`（全失败返 []）。不碰「设定即缓存」的正式副本 —— 那是用户已选定的图标。
   */
  refreshIconGalleries(): Promise<Array<{ name: string; url: string }>> {
    return invoke(IPC_CHANNELS.RULE_RESOURCES_REFRESH_ICON_GALLERIES);
  },
  onProgress(listener: (p: RuleResourceProgress) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_RULE_RESOURCE_PROGRESS, listener);
  },
};

// ============================================================================
// iconApi —— 自定义应用图标本地缓存（设定即下载，渲染零出站）
// ============================================================================

export const iconApi = {
  /**
   * 下载并缓存自定义应用图标，返回本地缓存 ref（`polaris-icon://c/<file>`）。
   * 只在用户「设定/更换图标」这一刻联网下载一次；成功后写进 preset.iconUrl，正常渲染永不触网。
   * 失败 throw（体积超限 / 非图片 / 网络错），调用方 catch 后回落存 remote URL（旧行为）。
   */
  cacheAppIcon(appId: string, remoteUrl: string): Promise<string> {
    return invoke(IPC_CHANNELS.CACHE_APP_ICON, { appId, remoteUrl });
  },
};

// ============================================================================
// ipInfoApi
// ============================================================================

export const ipInfoApi = {
  /** 获取出口 IP 快照。force=强制重测；visible=手动重探可见流程。 */
  async get(force = false, visible = false): Promise<IpInfoSnapshot> {
    return invoke(IPC_CHANNELS.IP_INFO_GET, { force, visible });
  },

  /** 纯读当前快照（零探测）：窗口重建后 store 为空时水合状态栏。绝不触发探测。 */
  async peek(): Promise<IpInfoSnapshot> {
    return invoke(IPC_CHANNELS.IP_INFO_GET, { peek: true });
  },

  onUpdated(listener: (snap: IpInfoSnapshot) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_IP_INFO_UPDATED, listener);
  },
};

// ============================================================================
// unlockApi —— 解锁检测（AI/流媒体），经当前代理出口。
// ============================================================================

export const unlockApi = {
  /** 跑一轮检测（force 绕 TTL，仍受 15s 硬下限约束）。 */
  async run(force = false): Promise<UnlockSnapshot> {
    return invoke(IPC_CHANNELS.UNLOCK_RUN, { force });
  },
  /** 纯读最近快照（页面挂载水合，零网络）；无则 null。 */
  async get(): Promise<UnlockSnapshot | null> {
    return invoke(IPC_CHANNELS.UNLOCK_GET);
  },
  /** 单个服务 settle 逐个点亮。 */
  onProgress(listener: (p: UnlockProgress) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_UNLOCK_PROGRESS, listener);
  },
  /** 切节点/起停代理 → 缓存失效。 */
  onInvalidated(listener: (p: UnlockInvalidatedPayload) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_UNLOCK_INVALIDATED, listener);
  },
  /** 一轮检测完成的完整终态快照。 */
  onUpdated(listener: (snap: UnlockSnapshot) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_UNLOCK_UPDATED, listener);
  },
};

// ============================================================================
// versionApi
// ============================================================================

export interface VersionInfo {
  appVersion: string;
  appName: string;
  buildDate: string;
  singBoxVersion: string;
  copyright: string;
  repositoryUrl: string;
  platform: string;
  arch: string;
  osVersion: string;
}

export const versionApi = {
  async getInfo(): Promise<VersionInfo> {
    return invoke(IPC_CHANNELS.VERSION_GET_INFO);
  },
};

// ============================================================================
// updateApi —— 应用更新
// ============================================================================

export interface UpdateCheckResult {
  hasUpdate: boolean;
  updateInfo?: UpdateInfo;
  error?: string;
}

export interface UpdateInfo {
  version: string;
  title: string;
  releaseNotes: string;
  downloadUrl: string;
  fileSize: number;
  publishedAt: string;
  isPrerelease: boolean;
  fileName: string;
  /**
   * GitHub release asset 的期望 sha256（由后端从 `digest` 字段解析）。
   * 存在时下载侧做强校验；旧 release 无该字段 → undefined，回落 Content-Length 校验。
   */
  sha256?: string;
}

/**
 * 安装前必须告知用户的事项（后端 `update_install::InstallAdvisory` 的 key）。
 *
 * 三者都是「OS 会拦一道，用户需要知道怎么点」——**应用内消不掉的必须提前讲清楚**：
 *  - `macosGatekeeper`：ad-hoc 签名 → 安装脚本会自动清 quarantine；万一失败需右键「打开」
 *  - `windowsSmartScreen`：无 Authenticode → 「更多信息 → 仍要运行」
 *  - `debElevation`：即将弹 polkit 提权框（取消即真 no-op，不会留下「代理被停但没更新」的坏态）
 */
export type InstallAdvisory = 'macosGatekeeper' | 'windowsSmartScreen' | 'debElevation';

/** `updateApi.install` 的返回：需确认 / 已交系统 / 已起安装脚本。 */
export interface UpdateInstallResult {
  ok: boolean;
  success?: boolean;
  /** true = 需要先向用户展示 advisory 说明，确认后再带 confirmed:true 重调。 */
  needConfirm?: boolean;
  advisory?: InstallAdvisory;
  /** 形态错配 → 已回退交系统打开（**不强制 root 安装**）。 */
  handedToSystem?: boolean;
  reason?: string;
  detail?: string;
}

export interface UpdateProgress {
  status:
    | 'idle'
    | 'checking'
    | 'no-update'
    | 'update-available'
    | 'downloading'
    | 'downloaded'
    | 'error';
  percentage: number;
  message: string;
  error?: string;
}

export const updateApi = {
  async check(includePrerelease = false): Promise<UpdateCheckResult> {
    return invoke(IPC_CHANNELS.UPDATE_CHECK, { includePrerelease });
  },

  /**
   * 下载更新包。
   *
   * `verified` 特指**摘要**这一级：`true` = 有期望 sha256 且逐字相符；`false` = 该 release 没给
   * 摘要（旧 release 的正常形态，不拒装），此时后端仍做了「清单 `fileSize` 等值 + Content-Length」
   * 两级弱校验。`digestSource` 如实标注摘要是谁给的，无摘要时为 `null` —— 出事时据它追责到具体
   * 信任根。复用本地已有包的那条路径同样带这个字段（它恰恰是靠这条摘要比中的）。
   *
   * 类型写成**字面量联合**而不是 `string`：后端的信任根是闭集（`DigestSource` 枚举，当前只有
   * `updater.rs` 的 `Self::GithubAssetDigest => "githubAssetDigest"` 一个变体）。将来 U3 的
   * `SHA256SUMS` 落地时会多一个来源，届时所有按来源分流的调用点必须被编译器点名——用 `string`
   * 就等于把那一刻本该编不过的地方全放过去。
   *
   * **本字段的后端实现在 `fix(updater): stream the app package to disk instead of buffering it`
   * 那一批**：本文件必须合在它之后，否则这段 JSDoc 是一份假契约（字段声明成可选 ⇒ tsc 全绿，
   * 而运行期拿到的是 `undefined` 不是 `null`，任何 `=== null` 的分支恒不成立）。
   */
  async download(updateInfo: UpdateInfo): Promise<{
    success: boolean;
    filePath?: string;
    verified?: boolean;
    digestSource?: 'githubAssetDigest' | null;
    error?: string;
  }> {
    return invoke(IPC_CHANNELS.UPDATE_DOWNLOAD, { updateInfo });
  },

  /**
   * 安装已下载的更新包。**两段式**：首调若返 `needConfirm`，UI 须先展示 `advisory` 说明，
   * 用户确认后带 `confirmed: true` 重调。确认框必须在停代理之前弹（取消 = 真 no-op）。
   */
  async install(filePath: string, confirmed = false): Promise<UpdateInstallResult> {
    return invoke(IPC_CHANNELS.UPDATE_INSTALL, { filePath, confirmed });
  },

  async skip(version: string): Promise<{ success: boolean }> {
    return invoke(IPC_CHANNELS.UPDATE_SKIP, { version });
  },

  async openReleases(): Promise<{ success: boolean }> {
    return invoke(IPC_CHANNELS.UPDATE_OPEN_RELEASES);
  },

  onProgress(listener: (progress: UpdateProgress) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_UPDATE_PROGRESS, listener);
  },
};

// ============================================================================
// coreUpdateApi —— 内核（sing-box）更新
// ============================================================================

/** 换核类命令的统一返回（`core_update_run` / `rollback` / `replaceManual` / `resetFactory` 共用）。 */
export interface CoreSwapResult {
  ok: boolean;
  result?: 'applied' | 'deferred' | 'noop';
  corePath?: string;
  hasBackup?: boolean;
  previousVersion?: string;
  currentVersion?: string;
  /** 换核前代理在跑 → 换完已自动重启。 */
  restarted?: boolean;
  /** 跨大版本带被自动更新硬闸拦下（手动换核可绕过）。 */
  crossBand?: boolean;
  latestVersion?: string;
}

export const coreUpdateApi = {
  async check(): Promise<{
    hasUpdate: boolean;
    currentVersion: string;
    currentVersionLine?: string;
    latestVersion?: string;
    downloadUrl?: string;
    assetName?: string;
    /** GitHub asset digest 解析出的期望 sha256（旧 release 可能缺）。 */
    sha256?: string | null;
    releaseNotes?: string;
    /** latestVersion 是否跨当前 minor 带；true 时 UI 标注跨大版本风险。 */
    crossBand?: boolean;
    error?: string;
  }> {
    return invoke(IPC_CHANNELS.CORE_UPDATE_CHECK);
  },

  /**
   * 下载并换核。传 downloadUrl 直接换；不传则后端自查一次。
   *
   * 返回结构化结果（**非 boolean**）：布尔会把 deferred/noop 折叠成「失败」，
   * 让「跨带被闸拦下」和「真失败」在 UI 上无从区分。
   */
  async update(downloadUrl?: string): Promise<CoreSwapResult> {
    // Polaris 原直接传裸 string；Tauri 需对象，底层包 { value }。
    return invokeScalar<CoreSwapResult>(IPC_CHANNELS.CORE_UPDATE_RUN, downloadUrl ?? '');
  },

  async getVersionInfo(): Promise<{
    currentVersion: string;
    bundledVersion: string;
    /**
     * 备份版本号。**恒为 null**：读它需执行 `<bak> version`（跑内核二进制），属真机腿。
     * `hasBackup` 已足以驱动「回滚」按钮；此处如实返 null 而非拿现役核版本冒充。
     */
    backupVersion: string | null;
    hasBackup: boolean;
    build: 'official' | 'fork' | 'unknown';
    pendingChangeNotice?: { previousVersion: string; currentVersion: string } | null;
  }> {
    return invoke(IPC_CHANNELS.CORE_GET_VERSION_INFO);
  },

  /** banner 展示版本变更通知后 ack 清除持久 pendingChangeNotice。 */
  async ackVersionChange(): Promise<void> {
    return invoke(IPC_CHANNELS.CORE_UPDATE_ACK_VERSION_CHANGE);
  },

  async rollback(): Promise<CoreSwapResult> {
    return invoke(IPC_CHANNELS.CORE_ROLLBACK);
  },

  onVersionChanged(
    listener: (data: {
      previousVersion: string;
      currentVersion: string;
      hasBackup: boolean;
    }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_CORE_VERSION_CHANGED, listener);
  },

  /**
   * 手动替换核心。无参：弹文件选择器 + 预检；传 { filePath, force:true }：跳过确认直接换。
   */
  async replaceManual(opts?: {
    filePath?: string;
    force?: boolean;
  }): Promise<
    | (CoreSwapResult & { ok: true; build?: CoreBuildKind })
    | {
        ok: false;
        /** 用户在系统文件选择器里取消 —— 正常流程，不是错误，UI 不得弹红。 */
        cancelled?: boolean;
        needConfirm?: boolean;
        sameVersion?: string;
        baselineOverride?: boolean;
        uploadVersion?: string;
        bundledVersion?: string;
        filePath?: string;
        error?: string;
      }
  > {
    return invoke(IPC_CHANNELS.CORE_REPLACE_MANUAL, opts);
  },

  /** 重置内核到随应用出厂的版本（不备份、清残留备份）。 */
  async resetFactory(): Promise<CoreSwapResult & { error?: string }> {
    return invoke(IPC_CHANNELS.CORE_RESET_FACTORY);
  },

  async getAutoStatus(): Promise<{
    /** 后端如实返 null（该开关的读取归 config 域，不在此猜 false）。 */
    autoUpdateCore: boolean | null;
    lastCheckAt: number | null;
    staged: { version: string; dir: string; stagedAt: string } | null;
    crossBandNotifiedVersion: string | null;
  }> {
    return invoke(IPC_CHANNELS.CORE_UPDATE_GET_AUTO_STATUS);
  },

  /**
   * 用户点「立即应用」：停代理→换核→重启（唯一允许主动断流）。
   *
   * 返回**五态对象**而非布尔：`discarded`（staged 已不领先/文件缺失）、`deferred`、`failed`
   * 各有不同处置，折叠成布尔会让 UI 把三者都误报成「已应用」（上游 修 M1 的原因）。
   */
  async applyStaged(): Promise<{
    result: 'applied' | 'discarded' | 'deferred' | 'failed' | 'noop';
    error?: string;
  }> {
    return invoke(IPC_CHANNELS.CORE_UPDATE_APPLY_STAGED);
  },

  onAutoStatusChanged(
    listener: (data: {
      lastCheckAt: number | null;
      staged: { version: string; stagedAt: string } | null;
      crossBandLatest: string | null;
    }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_CORE_AUTO_UPDATE_STATUS, listener);
  },
};

// ============================================================================
// subscriptionApi
// ============================================================================

export const subscriptionApi = {
  async add(
    subscription: Omit<SubscriptionConfig, 'id' | 'createdAt'>
  ): Promise<SubscriptionConfig> {
    return invoke(IPC_CHANNELS.SUBSCRIPTION_ADD, { subscription });
  },

  async update(subscription: SubscriptionConfig): Promise<void> {
    return invoke(IPC_CHANNELS.SUBSCRIPTION_UPDATE, { subscription });
  },

  async delete(subscriptionId: string): Promise<void> {
    return invoke(IPC_CHANNELS.SUBSCRIPTION_DELETE, { subscriptionId });
  },

  async updateServers(subscriptionId: string): Promise<{
    success: boolean;
    addedServers: number;
    updatedServers: number;
    deletedServers: number;
    error?: string;
    /** §16.3.4：304/无内容变化 → true（UI 弹「订阅无变化」toast）。 */
    unchanged?: boolean;
  }> {
    return invoke(IPC_CHANNELS.SUBSCRIPTION_UPDATE_SERVERS, { subscriptionId });
  },

  /** 订阅预检（add 前先行，不写 config）：拉取+解析返回节点数或分类错误。 */
  async preview(
    url: string,
    opts: { viaProxy?: boolean; userAgent?: string }
  ): Promise<SubscriptionPreviewResult> {
    return invoke(IPC_CHANNELS.SUBSCRIPTION_PREVIEW, {
      url,
      viaProxy: opts.viaProxy,
      userAgent: opts.userAgent,
    });
  },

  /**
   * 监听后台自动更新结果（scheduler 每个 due 订阅拉取后发一条）。渲染端仅对 `success:false` 弹 toast
   * （成功静默——对齐 上游 后台更新只入日志、不弹成功的 UX；手动刷新才三态 toast）。
   */
  onAutoUpdate(
    listener: (data: {
      subscriptionId: string;
      name: string;
      success: boolean;
      error?: string;
      addedServers?: number;
      updatedServers?: number;
      deletedServers?: number;
      unchanged?: boolean;
    }) => void
  ): () => void {
    return listen(IPC_CHANNELS.EVENT_SUBSCRIPTION_AUTOUPDATE, listener);
  },

  /**
   * 监听单订阅更新的逐阶段进度（手动刷新与后台 scheduler **共用**同一发射点）。
   * 消费点 = `store/use-subscription-progress-store.ts`（窗口级持久订阅 → 订阅信息栏）。
   */
  onUpdateProgress(listener: (data: SubscriptionUpdateProgress) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_SUBSCRIPTION_UPDATE_PROGRESS, listener);
  },
};

// ============================================================================
// localImportApi —— 本地导入（粘贴文本 / 文件）解析 + 系统文件选择
// ============================================================================

export const localImportApi = {
  /**
   * 解析粘贴文本 / 文件内容（离线，不联网）：识别 base64 / URL-list / Clash / sing-box，
   * 返回节点预览 + 统计。**0 节点 → 后端 throw**（IpcError，前端 catch 得错误文案）；
   * 不可识别格式亦 throw（ipc-channels.ts:45）。
   */
  async parse(text: string): Promise<ImportParseResult> {
    // Rust local_import_parse(text: String) —— 参数袋 key = `text`。
    return invoke(IPC_CHANNELS.LOCAL_IMPORT_PARSE, { text });
  },

  /**
   * 弹系统原生文件框（tauri-plugin-dialog）选配置文件 + 读内容回传。取消 → `canceled:true`；
   * 超限（10MB，同 `local_import_parse` 口径）/ 读失败 → `error`（`'too_large'|'read_failed'`）；
   * 成功 → `content` + `fileName`（basename，非全路径）。对齐 上游 `LOCAL_IMPORT_PICK_FILE`。
   */
  async pickFile(): Promise<{
    canceled: boolean;
    content?: string;
    fileName?: string;
    error?: string;
  }> {
    return invoke(IPC_CHANNELS.LOCAL_IMPORT_PICK_FILE);
  },
};

// ============================================================================
// backupApi
// ============================================================================

export interface BackupInfo {
  serverCount: number;
  manualServerCount: number;
  meshServerCount: number;
  subscriptionCount: number;
  ruleCount: number;
  ruleSetCount: number;
  appRuleCount: number;
  crossPlatformDisabledRules?: number;
}

export const backupApi = {
  /** 导出备份（按勾选类别；缺省/空 = 全部）。弹系统保存对话框。 */
  async export(
    categories?: BackupCategory[]
  ): Promise<{ success: boolean; filePath?: string; error?: string }> {
    return invoke(IPC_CHANNELS.BACKUP_EXPORT, { categories });
  },

  /** 导入①：弹文件框 + 解析 → 返回备份含哪些类 + 各类数量（不 apply）。canceled=用户取消。 */
  async importPick(): Promise<{
    canceled: boolean;
    filePath?: string;
    available?: BackupCategory[];
    counts?: Partial<Record<BackupCategory, number>>;
    error?: string;
  }> {
    return invoke(IPC_CHANNELS.BACKUP_IMPORT_PICK);
  },

  /** 导入②：按所选类整类替换 + 空跳过 + 保存。skipped=选了但备份为空被跳过的类。 */
  async importApply(
    filePath: string,
    categories: BackupCategory[]
  ): Promise<{
    success: boolean;
    info?: BackupInfo;
    skipped?: BackupCategory[];
    error?: string;
  }> {
    return invoke(IPC_CHANNELS.BACKUP_IMPORT_APPLY, { filePath, categories });
  },

  async getInfo(): Promise<BackupInfo> {
    return invoke(IPC_CHANNELS.BACKUP_GET_INFO);
  },
};

// ============================================================================
// diagnosticApi
// ============================================================================

export const diagnosticApi = {
  /** 导出诊断报告（弹出系统文件保存对话框，单 Markdown，密钥已脱敏）。 */
  async export(): Promise<{ success: boolean; filePath?: string; error?: string }> {
    return invoke(IPC_CHANNELS.DIAGNOSTIC_EXPORT);
  },

  // 此处曾有 captureStart / captureStop（「诊断采集」）。整条机制已删除 —— 内核日志改由管理 API 的
  // SubscribeLog 全级别送来、级别筛在客户端，把日志页级别拨到 DEBUG 即刻生效（不落盘、不重启内核）。
};

// ============================================================================
// helperApi —— macOS 提权 helper（免提权启停 sing-box）
// ============================================================================

export const helperApi = {
  async getStatus(force = false): Promise<HelperStatus> {
    // Rust helper_get_status(_force: Option<bool>) —— 参数袋 key = `force`（**非** `value`）。
    // Option 参数缺失不崩，但 invokeScalar 的 { value } 让 force 永远传不进后端。
    return invoke(IPC_CHANNELS.HELPER_GET_STATUS, { force });
  },

  /** 安装/修复 helper（弹一次管理员授权框）。 */
  async install(): Promise<{ success: boolean; error?: string; status: HelperStatus }> {
    return invoke(IPC_CHANNELS.HELPER_INSTALL);
  },

  /** 卸载 helper（弹一次管理员授权框）。 */
  async uninstall(): Promise<{
    success: boolean;
    error?: string;
    status: HelperStatus;
  }> {
    return invoke(IPC_CHANNELS.HELPER_UNINSTALL);
  },

  /** 监听「helper 可升级」事件。 */
  onUpgradeable(listener: (data: { version: string }) => void): () => void {
    return listen(IPC_CHANNELS.EVENT_HELPER_UPGRADEABLE, listener);
  },
};

// ============================================================================
// appApi
// ============================================================================

/**
 * 完全卸载的各类目标（= Rust `runtime::uninstall::UninstallStep`，serde camelCase）。
 *
 * 数组顺序即**因果执行序**，理由见 Rust 侧模块文档：先停核 → 卸 helper（它要用配置目录）→
 * 删配置 → 清更新缓存 → 清应用偏好域（macOS `~/Library/Preferences/<id>.plist`；本进程还在跑，
 * 越晚清回写窗口越小）→ 最后删应用本体（它是当前进程的载体）。
 */
export type UninstallStep =
  | 'stopCore'
  | 'autostart'
  | 'helper'
  | 'userConfig'
  | 'cacheDir'
  | 'preferences'
  | 'appBundle';

/**
 * 单步结果（= Rust `StepOutcome`，`#[serde(tag = "kind")]`）。
 *
 * **五态而非布尔**：`skipped`（本就无事可做）、`unsupported`（本平台做不到）、
 * `notAttempted`（因前一步失败而没试）三者语义完全不同，糊成 `false` 就是骗人。
 */
export type UninstallOutcomeKind = 'done' | 'skipped' | 'unsupported' | 'failed' | 'notAttempted';

export interface UninstallStepReport {
  step: UninstallStep;
  outcome: { kind: UninstallOutcomeKind; detail: string };
}

/**
 * 逐项卸载报告（= Rust `UninstallReport`）。
 *
 * ⚠️ **`verdict` 才是真值，不是外层信封的 `success`**。外层恒 `success:true`（IPC 层没失败），
 * 因为 `ipc-client` 在 `success:false` 时 throw 且会丢掉 `data` —— 而逐项结果正是必须呈现的东西。
 * 只有 `complete` 能显示成「已卸载」；`incomplete`/`failed` 必须把剩下要用户手动做的事摆出来。
 */
export interface UninstallReport {
  steps: UninstallStepReport[];
  verdict: 'complete' | 'incomplete' | 'failed';
  /** 配置或应用本体已被真删 ⇒ 当前进程赖以运行的东西没了，应引导退出。 */
  requiresExit: boolean;
}

export const appApi = {
  /**
   * B6：完全卸载 Polaris（提权 helper / 受保护目录内核 / 用户配置 / 应用本体）。
   *
   * **不 throw 就等于卸载成功是错的** —— 判据是 `report.verdict === 'complete'`。
   */
  async uninstallAll(): Promise<UninstallReport> {
    return invoke(IPC_CHANNELS.APP_UNINSTALL_ALL);
  },

  /** 打开 sing-box 官方面板。代理未运行 → 返回 { ok: false }。 */
  async openSingboxDashboard(locale?: string): Promise<{ ok: boolean }> {
    // Rust open_singbox_dashboard(_locale: Option<String>) —— 参数袋 key = `locale`（**非** `value`）。
    // Option 参数缺失不崩，但 invokeScalar 的 { value } 让 locale 永远传不进后端。
    return invoke(IPC_CHANNELS.OPEN_SINGBOX_DASHBOARD, { locale });
  },

  /** 刷新 sing-box 官方面板资源：清本地缓存目录。 */
  async refreshSingboxDashboard(): Promise<{ ok: boolean }> {
    return invoke(IPC_CHANNELS.REFRESH_SINGBOX_DASHBOARD);
  },

  /** dashboard #55：取面板连接信息（url + apiUrl + secret）。 */
  async getSingboxDashboardConnection(): Promise<{
    ok: boolean;
    url: string;
    apiUrl: string;
    secret: string;
  }> {
    return invoke(IPC_CHANNELS.GET_SINGBOX_DASHBOARD_CONNECTION);
  },
};

// ============================================================================
// 聚合导出（与 Polaris `api` 形状完全一致，组件 import { api } from '@/ipc' 沿用）
// ============================================================================

export const api = {
  proxy: proxyApi,
  config: configApi,
  privacy: privacyApi,
  server: serverApi,
  rules: rulesApi,
  logs: logsApi,
  autoStart: autoStartApi,
  stats: statsApi,
  connections: connectionsApi,
  system: systemApi,
  ruleResources: ruleResourcesApi,
  icon: iconApi,
  ipInfo: ipInfoApi,
  version: versionApi,
  update: updateApi,
  coreUpdate: coreUpdateApi,
  subscription: subscriptionApi,
  localImport: localImportApi,
  backup: backupApi,
  diagnostic: diagnosticApi,
  helper: helperApi,
  app: appApi,
  window: windowApi,
};

export default api;
