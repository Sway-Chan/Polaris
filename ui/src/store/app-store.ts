/**
 * Polaris 主应用 store（Zustand）—— Polaris app-store 的底座骨架移植。
 *
 * 迁移策略（底座阶段）：
 *  - **状态结构忠实对齐 Polaris**（字段名/类型不变），后续阶段补组件时 store 形状已就位、可直接订阅。
 *  - **核心 action 保留**：loadConfig / saveConfig / startProxy / stopProxy / switchServer 等。
 *  - **非底座 action 推迟**：Polaris app-store 1066 行中大量与具体组件交互（测速流、解锁检测流、
 *    privacy 模式计时、托盘联动等）的 action 在底座阶段不移植——它们的消费者组件尚未存在。
 *    保留对应 state 字段（占位空值），action 随组件阶段补。
 *  - 与 Polaris 的差异：Polaris 经 Electron ipcClient（{success,data} 信封），Polaris 经 Tauri api（raw 值）。
 *    api-client 已抹平此差异，store 层无感知。
 */

import { create } from 'zustand';
import type {
  UserConfig,
  ProxyStatus,
  Rule,
  TrafficStats,
  HelperStatus,
  IpInfoSnapshot,
  InvalidNodeInfo,
  ServerConfig,
} from '../contracts/types';
import type { UnlockResult, UnlockEgress, UnlockSnapshot } from '../contracts/unlock-detection';
import type { TailscaleStatusEvent } from '../contracts/tailscale-status';
import { api } from '../ipc';
import { withConfigWriteLock } from '../lib/config-write-lock';
import { replay, type StagedEntry } from '../lib/staged-config';
import { hydrateStagedConfig, useStagedConfigStore } from './staged-config-store';
import {
  loadTailscaleLoginStatesFromCache,
  useTailscaleLoginCacheStore,
} from './use-tailscale-login-cache-store';

// 兼容旧的类型别名（Polaris 习惯）
type ProxyMode = UserConfig['proxyMode'];

/**
 * 可用的内核更新（常驻入口数据源）。放 store 使 CoreManagementCard 卸载后入口仍留存。
 */
export interface AvailableCoreUpdate {
  latestVersion: string;
  downloadUrl: string;
  /** 是否跨当前 minor 带；true 时 UI 用警告色 + 风险文案。 */
  crossBand?: boolean;
}

/**
 * 解锁检测显示态：提到 store 使其跨首页组件卸载存活（切导航离开 → 卸载但检测态留存，切回直接展示不重跑）。
 */
export interface UnlockDisplayState {
  results: Record<string, UnlockResult>;
  running: boolean;
  checkedAt: number | null;
  egress: UnlockEgress | null;
  lastRunAt: number | null;
}

/**
 * §2 待应用差集显示态（`proxy:getPendingChanges` pull + `event:proxyPendingChanges` push 的 store 落点）。
 *
 * 形状与后端 `PendingChangesSummary` / 前端契约 [`PendingNodeChanges`] 逐字段一致
 * （pull 与 push 同构，后端无适配层）。
 *
 * 需要**即时**差集的场景（如 willRestartOnSelect 选节点预判）仍走 pull：切节点会触发重启清差集，
 * 读 store 快照可能被 push 更新滞后一拍，pull 拿的是当下真值。
 */
export interface PendingChangesState {
  added: string[];
  modified: string[];
  removed: string[];
  /** 「保存不重启」降级下来的非节点结构性变更（三个数组看不见它，见 `PendingNodeChanges`）。 */
  restartDeferred: boolean;
}

/**
 * 跨屏一次性意图（契约「主页 Home · 空状态」：无节点 → 「添加节点/添加订阅」跳 server 页并**携 action**）。
 *
 * 为什么要一个 store 字段而不是「跳过去让用户自己再点一次添加」：空状态那两个按钮的语义就是
 * 「我要加一个节点/订阅」，落到节点页却只看到同样空的列表 = 意图在导航途中被丢掉，用户得在陌生页面
 * 重新找入口。对齐 上游 `serverPageAction`（`renderer/store/app-store.ts:139`，消费点
 * `pages/server-page.tsx:178-191`）。
 *
 * **一次性**：消费方（NodesScreen）读到后必须立刻置回 null，否则用户手动关掉对话框、切走再切回节点页时
 * 会被同一个意图反复弹窗。
 */
export type ServerPageAction = 'add-server' | 'add-sub';

// loadConfig 单飞：防 configChanged 风暴 / 启动期重复拉取（替代原 isLoading 重入守卫）
let loadConfigInflight: Promise<void> | null = null;
// loadConfig 代际计数：mutation 成功后自增，使在飞的旧 load 回填被丢弃（避免用旧快照覆盖 store）。
let loadConfigGeneration = 0;

function invalidateLoadConfig(): void {
  loadConfigGeneration++;
  loadConfigInflight = null;
}

export interface AppState {
  // ── 配置 ──
  config: UserConfig | null;
  configLoading: boolean;
  configError: string | null;

  // ── 代理运行态 ──
  proxyStatus: ProxyStatus | null;
  proxyStarting: boolean;
  proxyStopping: boolean;

  // ── 节点 / 订阅 ──
  servers: ServerConfig[];
  selectedServerId: string | null;
  tailscaleLoginStates: Record<string, boolean>;
  // serverId → 交互登录 URL。瞬态登录核经 EVENT_TAILSCALE_AUTH_URL 回填；1.14 主核也会为 NeedsLogin 的
  // endpoint always-emit。放 store 而非弹窗本地 state：卡片角标要据此判「登录中」，弹窗关了它也得在。
  tailscaleAuthUrls: Record<string, string>;
  // serverId → 用户是否**显式发起**了登录（TsLoginDialog 提交即置位，弹窗收尾清除）。
  // 与 authUrl 分开：主核 always-emit 会给未选中/未就绪节点持续推 URL，只凭 hasAuthUrl 判「登录中」会误报。
  tailscaleLoginInitiated: Record<string, boolean>;

  // ── 规则 ──
  rules: Rule[];

  // ── 流量统计 / 连接（订阅驱动，底座占位）──
  trafficStats: TrafficStats | null;

  // ── 出口 IP ──
  ipInfo: IpInfoSnapshot | null;

  // ── 解锁检测 ──
  unlock: UnlockDisplayState;

  // ── §2 待应用差集 ──
  pendingChanges: PendingChangesState;
  invalidNodes: InvalidNodeInfo[];

  // ── helper（macOS 提权）──
  helperStatus: HelperStatus | null;

  // ── 可用更新 ──
  availableCoreUpdate: AvailableCoreUpdate | null;

  // ── 隐私模式 ──
  privacyMode: boolean;

  // ── Tailscale 节点详情：STATUS 流推送的**整帧末态**（serverId → 末帧）──
  // 存整帧而非只存 `peers`：出口告警要同时读 `peers`（离线/未广告）与 `backendState`/`expired`
  // （未认证），两者必须来自**同一帧**，拆成两个切片会出现「peers 是第 N 帧、认证态是第 M 帧」的
  // 撕裂态。`loggedIn` 另有 `tailscaleLoginStates`（缓存+state 目录+STATUS 三源折叠的**综合**态，
  // 代理关时也有值）——本切片是**未折叠的原始帧**，只在核跑着时有，两者不互相替代。
  tailscaleStatuses: Record<string, TailscaleStatusEvent | undefined>;

  // ── 跨屏一次性意图（Home 空状态 → Nodes 页自动唤起对应对话框）──
  serverPageAction: ServerPageAction | null;
  // ── 跨屏一次性意图（设置页「在主页切换 →」→ Home 页高亮落点，原型 :4052）──

  // ── 动作 ──
  loadConfig: (force?: boolean) => Promise<void>;
  saveConfig: (config: UserConfig) => Promise<void>;
  updateProxyMode: (mode: ProxyMode) => Promise<void>;
  startProxy: () => Promise<void>;
  stopProxy: () => Promise<void>;
  switchServer: (serverId: string) => Promise<void>;
  setProxyStatus: (status: ProxyStatus | null) => void;
  refreshProxyStatus: () => Promise<void>;
  setTrafficStats: (stats: TrafficStats | null) => void;
  setIpInfo: (info: IpInfoSnapshot | null) => void;
  setUnlock: (partial: Partial<UnlockDisplayState>) => void;
  setUnlockProgress: (serviceId: string, result: UnlockResult) => void;
  applyUnlockSnapshot: (snap: UnlockSnapshot) => void;
  beginUnlockCheck: () => void;
  resetUnlock: () => void;
  setPendingChanges: (changes: PendingChangesState) => void;
  setInvalidNodes: (nodes: InvalidNodeInfo[]) => void;
  setPrivacyMode: (enabled: boolean) => void;
  setTailscaleLoginState: (serverId: string, loggedIn: boolean) => void;
  applyTailscaleStateExists: (states: Record<string, boolean>) => void;
  setTailscaleAuthUrl: (serverId: string, url: string | null) => void;
  setTailscaleStatus: (event: TailscaleStatusEvent) => void;
  setTailscaleLoginInitiated: (serverId: string, initiated: boolean) => void;
  setServerPageAction: (action: ServerPageAction | null) => void;
  reset: () => void;
}

const EMPTY_PENDING: PendingChangesState = {
  added: [],
  modified: [],
  removed: [],
  restartDeferred: false,
};

const initialUnlock: UnlockDisplayState = {
  results: {},
  running: false,
  checkedAt: null,
  egress: null,
  lastRunAt: null,
};

export const useAppStore = create<AppState>((set, get) => ({
  config: null,
  configLoading: false,
  configError: null,

  proxyStatus: null,
  proxyStarting: false,
  proxyStopping: false,

  servers: [],
  selectedServerId: null,
  // 启动秒显：从 localStorage 缓存恢复「登录过没」布尔表（代理关时的兜底显示）
  tailscaleLoginStates: loadTailscaleLoginStatesFromCache(),
  tailscaleAuthUrls: {},
  tailscaleLoginInitiated: {},

  rules: [],

  trafficStats: null,
  ipInfo: null,

  unlock: initialUnlock,

  pendingChanges: EMPTY_PENDING,
  invalidNodes: [],

  helperStatus: null,
  availableCoreUpdate: null,
  privacyMode: false,
  tailscaleStatuses: {},
  serverPageAction: null,

  loadConfig: async (force?: boolean) => {
    // 单飞：已在飞且非 force → 复用（防 configChanged 风暴）
    if (!force && loadConfigInflight) {
      return loadConfigInflight;
    }

    const myGeneration = ++loadConfigGeneration;
    set({ configLoading: true, configError: null });

    loadConfigInflight = (async () => {
      try {
        const config = await api.config.get();
        // 代际比对：mutation 成功后自增代际，在飞的旧 load（拿的是 mutation 前旧快照）回填被丢弃
        if (myGeneration !== loadConfigGeneration) return;

        set({
          config,
          servers: config.servers ?? [],
          selectedServerId: config.selectedServerId,
          rules: config.customRules ?? [],
          configLoading: false,
          configError: null,
        });

        // 暂存层的 baseline 版本跟随磁盘现值（首次还负责从渲染端存储恢复未保存的编辑）。
        // 总开关关时本调用连 store 都不碰，是逐字节的 no-op。
        hydrateStagedConfig(config);
      } catch (err) {
        if (myGeneration !== loadConfigGeneration) return;
        const message = err instanceof Error ? err.message : String(err);
        set({ configLoading: false, configError: message });
        console.error('[app-store] loadConfig failed:', err);
      } finally {
        if (myGeneration === loadConfigGeneration) {
          loadConfigInflight = null;
        }
      }
    })();

    return loadConfigInflight;
  },

  // **整份覆盖写**，且**刻意不传 `baseVersion`**（= 不做乐观并发校验）：调用方
  // （HomeScreen 出口哨兵 / 接管方式、RulesScreen 地区分流、LogsScreen 非暂存腿）都是「用户刚点了
  // 这个动作」，语义是最后写入者胜，加校验只会让它在有人同时编辑时**静默失败**——把一个缺陷换成
  // 另一个（陈述见交付说明）。
  //
  // 但它必须与 `performSave` **共用同一条写队列**：两者在同一 webview 内，若本函数落在
  // `performSave` 的 `get()`→`save()` 窗口中间，就会顶高盘版本、把那次带 `baseVersion` 的保存
  // 撞成 conflict ⇒ 条报「保存失败」而盘其实好着。进队列后那个窗口不再存在。
  saveConfig: async (config) => {
    try {
      await withConfigWriteLock(() => api.config.save(config));
      invalidateLoadConfig();
      set({
        config,
        servers: config.servers ?? [],
        selectedServerId: config.selectedServerId,
        rules: config.customRules ?? [],
      });
    } catch (err) {
      console.error('[app-store] saveConfig failed:', err);
      throw err;
    }
  },

  updateProxyMode: async (mode) => {
    const { config } = get();
    if (!config) return;
    await api.config.updateMode(mode);
    invalidateLoadConfig();
    set({ config: { ...config, proxyMode: mode } });
  },

  startProxy: async () => {
    // `config` 只用来判「配置加载完了没」（没加载完 = UI 还没准备好让用户点连接）。
    // **不再作为起核载荷下发**：起核用哪份配置由后端读盘决定（见 Rust `proxy_start` 头注）——
    // 这份内存副本靠 configChanged 异步刷新，「写盘 → 立刻点启动」会用写之前的那份。
    if (!get().config) {
      throw new Error('Cannot start proxy: config not loaded');
    }
    set({ proxyStarting: true });
    try {
      await api.proxy.start();
    } finally {
      set({ proxyStarting: false });
    }
  },

  stopProxy: async () => {
    set({ proxyStopping: true });
    try {
      await api.proxy.stop();
    } finally {
      set({ proxyStopping: false });
    }
  },

  /** 切节点。**必须同写 `config.selectedServerId`**：`startProxy`(:264) 发给后端的是整个 `config`，
   * 只更新扁平 `selectedServerId` 会让 `config` 里留着旧值（如 `__direct__`）→ 起核按旧值落**直连**，
   * 而 UI 仍显示新节点 = 用户以为走代理、实则明文直连（真机实证的静默直连根因）。两处同源必须同写。 */
  switchServer: async (serverId) => {
    const { config } = get();
    await api.server.switch(serverId);
    invalidateLoadConfig();
    set({
      selectedServerId: serverId,
      ...(config ? { config: { ...config, selectedServerId: serverId } } : {}),
    });
  },

  setProxyStatus: (status) => set({ proxyStatus: status }),

  /** 从后端重拉连接态。事件驱动的唯一刷新入口（App.tsx 的 proxyStarted/proxyStopped 订阅调它）——
   * `startProxy`/`stopProxy` 自身不写 `proxyStatus`，因为托盘浮层（独立窗口，不共享本 store）也能
   * 触发启停，只有走事件才能让两个窗口收敛到同一真值。失败不抛：连接态刷新失败不该冒泡成 UI 异常。 */
  refreshProxyStatus: async () => {
    try {
      const status = await api.proxy.getStatus();
      set({ proxyStatus: status ?? null });
    } catch (err) {
      console.error('[app-store] refreshProxyStatus failed:', err);
    }
  },
  setTrafficStats: (stats) => set({ trafficStats: stats }),
  setIpInfo: (info) => set({ ipInfo: info }),
  setUnlock: (partial) =>
    set((state) => ({ unlock: { ...state.unlock, ...partial } })),

  /**
   * 单服务 settle 逐个点亮 —— **merge 进 results，不整体替换**（对齐 上游 `setUnlockProgress`
   * `app-store.ts:554-557`）。
   *
   * 曾经的写法是 `setUnlock({ results: { [id]: result } })`：`setUnlock` 只浅合并**顶层字段**，`results`
   * 整个被换成单键对象 ⇒ 每收到一条 progress 就抹掉另外五项，六个徽章只有最后 settle 的那个留得住。
   */
  setUnlockProgress: (serviceId, result) =>
    set((state) => ({
      unlock: { ...state.unlock, results: { ...state.unlock.results, [serviceId]: result } },
    })),

  /**
   * 应用一份终态快照（`unlock:run` 返回值 / `EVENT_UNLOCK_UPDATED` / 挂载水合共用），对齐 上游
   * `applyUnlockSnapshot`（`app-store.ts:558-600`）。
   *
   * **为何必须消费 `run()` 的返回值**：后端有若干条早退路径（gating 短路 / S-gate notReady / TTL 命中 /
   * force 硬下限）——它们都 emit 了终态，但把返回值丢掉的调用方会把 `running:true` 永远留在 store 上。
   * 三态分流按 上游 逐条对齐：
   * - `blockedReason`（代理没跑 / 出口无效）→ 复位 idle，**不打 lastRunAt**（毫秒级短路，不该触发 15s 冷却）；
   * - `notReady`（就绪门未过 / 整轮预算耗尽）→ idle **但打戳**（后端已置 lastRunAt，force 硬下限已生效，冷却须镜像）；
   * - 真检测落定 → 落 results/checkedAt/egress，`lastRunAt` 取 `snap.checkedAt`（**后端真跑那一轮的完成时刻**，
   *   而非 `Date.now()`）——TTL 命中回放的是旧 checkedAt，用它派生冷却才不会把 15s 前的旧检测误算成刚跑完。
   */
  applyUnlockSnapshot: (snap) =>
    set((state) => {
      // 陈旧轮 no-op 快照（空 results + 无终态标记）：本轮在飞期间被 invalidate 已被后端丢弃，
      // 新一轮可能已 beginUnlockCheck 接管显示 → 不覆盖，否则把新一轮的「检测中」清成空 idle。
      if (
        !snap.checkedAt &&
        !snap.notReady &&
        !snap.blockedReason &&
        Object.keys(snap.results).length === 0
      ) {
        return { unlock: state.unlock };
      }
      if (snap.blockedReason) {
        return {
          unlock: { results: {}, running: false, checkedAt: null, egress: null, lastRunAt: null },
        };
      }
      if (snap.notReady) {
        return {
          unlock: {
            results: {},
            running: false,
            checkedAt: null,
            egress: null,
            lastRunAt: Date.now(),
          },
        };
      }
      return {
        unlock: {
          results: snap.results,
          running: false,
          checkedAt: snap.checkedAt,
          egress: snap.egress,
          lastRunAt: snap.checkedAt ?? Date.now(),
        },
      };
    }),

  /** 置「检测中」（六徽章转圈）；保留 lastRunAt——冷却窗由上一次**完成**时刻派生，不因新一轮开跑而重置。 */
  beginUnlockCheck: () =>
    set((state) => ({
      unlock: {
        results: {},
        running: true,
        checkedAt: null,
        egress: null,
        lastRunAt: state.unlock.lastRunAt,
      },
    })),

  /** 复位 idle（停代理 / IPC 失败兜底）：连 lastRunAt 一并清，重连后不被陈旧冷却误锁刷新钮。 */
  resetUnlock: () => set({ unlock: initialUnlock }),
  setPendingChanges: (changes) => set({ pendingChanges: changes }),
  setInvalidNodes: (nodes) => set({ invalidNodes: nodes }),
  setPrivacyMode: (enabled) => set({ privacyMode: enabled }),

  /**
   * 写入某 TS 节点的登录态真值（STATUS 流 / state 清除事件的唯一入口）。
   *
   * 双写：内存态供 UI 即时渲染；localStorage 缓存供下次启动秒显——代理关时没有常驻核 STATUS 流，
   * 缓存是那时唯一能秒显的来源（see use-tailscale-login-cache-store 顶注）。
   */
  setTailscaleLoginState: (serverId, loggedIn) => {
    if (!serverId) return;
    useTailscaleLoginCacheStore.getState().setCached(serverId, loggedIn);
    const prev = get().tailscaleLoginStates;
    if (prev[serverId] === loggedIn) return; // 值未变 → 不新建对象，省订阅者重渲染
    set({ tailscaleLoginStates: { ...prev, [serverId]: loggedIn } });
  },

  /**
   * 挂载兜底：用 state 目录存在性批量校正「登录过没」（TAILSCALE_STATE_EXISTS，不起核）。
   *
   * 不写 localStorage 缓存：缓存的语义是「上次 STATUS 已知态」，而这里只是文件系统层面的粗粒度兜底
   * （state 在 ≠ key 没过期）。与 STATUS 的竞态（挂载时代理已在跑）无需额外守卫：STATUS 是持续流，
   * 下一帧即把真值盖回来。
   */
  applyTailscaleStateExists: (states) => {
    const prev = get().tailscaleLoginStates;
    const next = { ...prev };
    let changed = false;
    for (const [id, exists] of Object.entries(states)) {
      if (next[id] !== exists) {
        next[id] = exists;
        changed = true;
      }
    }
    if (changed) set({ tailscaleLoginStates: next });
  },

  /** 写入/清除某节点的交互登录 URL（null = 清除，如重新发起登录前抹掉上一轮的陈旧 URL）。 */
  setTailscaleAuthUrl: (serverId, url) => {
    if (!serverId) return;
    const prev = get().tailscaleAuthUrls;
    if ((prev[serverId] ?? null) === url) return;
    const next = { ...prev };
    if (url === null) delete next[serverId];
    else next[serverId] = url;
    set({ tailscaleAuthUrls: next });
  },

  /**
   * 落 STATUS 末帧（每帧全量覆盖该 serverId）。
   *
   * **与 [`setTailscaleLoginState`] 的分工**：那条是折叠登录态的**判决门**（过
   * `isDefinitiveTsLoginFrame`，非终局帧不许翻转、且双写 localStorage）；本条是**原始帧留存**，
   * 不过门、不写缓存 —— 出口告警要的正是「控制面这一刻怎么说」（`backendState` / `expired` /
   * `peers`），过一道折叠门反而把判据抹掉了。两条从同一个订阅各取所需。
   *
   * 逐字段比对早退（`backendState`/`expired`/`authURL` + peers 全等）：STATUS 是 ~1/s 的持续流，
   * 无脑 `set` 会让每个订阅者每秒重渲染一次。
   */
  setTailscaleStatus: (event) => {
    if (!event?.serverId) return;
    const prev = get().tailscaleStatuses;
    const old = prev[event.serverId];
    if (
      old &&
      old.backendState === event.backendState &&
      old.loggedIn === event.loggedIn &&
      old.expired === event.expired &&
      old.authURL === event.authURL &&
      old.peers.length === event.peers.length &&
      old.peers.every((p, i) => {
        const n = event.peers[i];
        return (
          p.hostName === n.hostName &&
          p.ip === n.ip &&
          p.online === n.online &&
          p.exitNode === n.exitNode &&
          p.exitNodeOption === n.exitNodeOption
        );
      })
    ) {
      return;
    }
    set({ tailscaleStatuses: { ...prev, [event.serverId]: event } });
  },

  /** 标记/清除「用户显式发起的登录在飞」。 */
  setTailscaleLoginInitiated: (serverId, initiated) => {
    if (!serverId) return;
    const prev = get().tailscaleLoginInitiated;
    if ((prev[serverId] ?? false) === initiated) return;
    const next = { ...prev };
    if (initiated) next[serverId] = true;
    else delete next[serverId];
    set({ tailscaleLoginInitiated: next });
  },

  /** 置/清跨屏一次性意图。消费方读到后立刻 `setServerPageAction(null)`（见类型注释）。 */
  setServerPageAction: (action) => set({ serverPageAction: action }),

  /** 同上，首页版（消费点 HomeScreen 的高亮 effect）。 */

  reset: () =>
    set({
      config: null,
      configLoading: false,
      configError: null,
      proxyStatus: null,
      proxyStarting: false,
      proxyStopping: false,
      servers: [],
      selectedServerId: null,
      rules: [],
      trafficStats: null,
      ipInfo: null,
      unlock: initialUnlock,
      pendingChanges: EMPTY_PENDING,
      invalidNodes: [],
      helperStatus: null,
      availableCoreUpdate: null,
      privacyMode: false,
      // 原始帧只在核跑着时有意义（reset 语义 = 回到「什么都不知道」），清掉即回落到
      // 「无帧 → 出口告警不据陈旧帧下认证/离线结论」。
      tailscaleStatuses: {},
      // 一次性跨屏意图是瞬时态：reset 后没人再该被它弹窗/高亮。
      serverPageAction: null,
          // 登录中的瞬时态随 reset 清掉；tailscaleLoginStates 刻意不清——它是缓存/state 文件支撑的持久登录态，
      // 不随一次 app 状态重置而「变成未登录」。
      tailscaleAuthUrls: {},
      tailscaleLoginInitiated: {},
    }),
}));

// ─────────────────────────── effectiveConfig（暂存回显的唯一读点）───────────────────────────

/**
 * 「用户现在应该看到的那份配置」= `replay(磁盘 config, staged 条目)`（spec §2.5 Q2 的 `effectiveConfig`）。
 *
 * # 为什么必须有这层，而不是让展示点直接读 `config`
 *
 * `config` 是**磁盘真值**。暂存层一开，用户改完节点、对话框关掉，列表读 `config` 就还是旧值 ——
 * 那不是「延迟生效」，是 UI 撒谎，与 §Q3 否决「切节点进暂存」的理由完全同型（列表高亮 A、
 * 状态栏显示 A，而流量走 B）。展示可编辑实体的读点一律经本层；**只有回答「磁盘上是什么」的读点**
 * （暂存基准 / 落盘入参 / 与后端对账）才继续读裸 `config`，判据表见 `lib/config-read-wiring.test.ts`。
 *
 * # 引用稳定性是硬要求，不是优化
 *
 * zustand 的 selector 按引用比较：每次返回一个新对象 = 全应用每次 store 变更都重渲染。故：
 *  - **条目为空 ⇒ 原样返回入参 `config` 本身**（不是深拷贝的等值对象）。`STAGED_CONFIG_ENABLED`
 *    为 `false` 时条目恒空 ⇒ 本层在产品行为与渲染开销上都是逐字节零变化。
 *  - 条目非空 ⇒ 单槽记忆化，**失效判据取入参的引用**（`config` / `entries` 任一换了才重算）。
 *    不拿 `JSON.stringify` 当 key：config 与 `config.json` 同量级，每次渲染序列化一遍比 `replay` 还贵；
 *    两个入参都由各自的 store 不可变替换（`set({config})` / `stageEntry` 恒建新数组），引用即是判据。
 *
 * `replay` 对畸形条目**跳过而非抛**（`lib/staged-config.ts` 的 `applyEntry`）—— 本函数在渲染路径上，
 * 抛 = 白屏，故包装层也不得引入新的抛点。
 */
let memoConfig: UserConfig | null = null;
let memoEntries: readonly StagedEntry[] | null = null;
let memoResult: UserConfig | null = null;

export function effectiveConfigOf(
  config: UserConfig | null,
  entries: readonly StagedEntry[]
): UserConfig | null {
  if (config === null || entries.length === 0) return config;
  if (config === memoConfig && entries === memoEntries) return memoResult;
  memoConfig = config;
  memoEntries = entries;
  memoResult = replay(config, entries);
  return memoResult;
}

const identity = (c: UserConfig | null): unknown => c;

/**
 * 订阅式读点。**默认取整份**（`useEffectiveConfig()`），传 selector 可保持今天的窄订阅粒度：
 * `useEffectiveConfig((c) => c?.uiTheme)` 与原来的 `useAppStore((s) => s.config?.uiTheme)` 一样
 * 只在该字段变化时重渲染 —— 改读本 hook **不放大**任何组件的重渲染面。
 *
 * 两个订阅缺一不可：条目变了要重渲（暂存的编辑要立刻回显），config 变了也要（磁盘侧被别处改了）。
 */
export function useEffectiveConfig<T = UserConfig | null>(
  select: (c: UserConfig | null) => T = identity as (c: UserConfig | null) => T
): T {
  const entries = useStagedConfigStore((s) => s.entries);
  return useAppStore((s) => select(effectiveConfigOf(s.config, entries)));
}

/** 命令式读点（事件回调 / 一次性快照）。与 `useEffectiveConfig` 同一记忆化槽。 */
export function getEffectiveConfig(): UserConfig | null {
  return effectiveConfigOf(
    useAppStore.getState().config,
    useStagedConfigStore.getState().entries
  );
}

// ───────────────────── 磁盘镜像集合的合成视图（列表回显）─────────────────────

/**
 * `servers` / `rules` 是 app-store 从 `config.servers` / `config.customRules` 抄出来的**扁平磁盘镜像**
 * （`loadConfig` / `saveConfig` 各抄一次）—— 节点页、规则页、首页出口选单真正渲染的是它们，
 * 不是 `config`。故只把 `config` 读点接上 `effectiveConfig` **不会**让列表回显 staged 编辑：
 * 那条腿仍在读盘。本层补的就是这一段。
 *
 * # 判据：同一集合上的两个不同问题（handoff 已裁定，勿重新选型）
 *
 * | 读点在问什么 | 读谁 |
 * |---|---|
 * | 「用户**现在编辑出来**的实体集合长什么样」（列表、编辑基准、下拉枚举） | effective |
 * | 「**后端此刻能按 id 找到 / 正在使用**的实体集合长什么样」（出口选单、切节点、按 id 下发的 IPC） | disk 镜像 |
 *
 * 后者读 effective 必错：staged-only 实体在盘上不存在，后端按 id 查不到 ⇒ 拒绝。
 * 前者读 disk 必错：用户看不见自己刚做的编辑 ⇒ UI 撒谎（与 spec §2.5 Q3 否决「切节点进暂存」同型）。
 * ⇒ staged-only 实体**在列表里可见且带「待保存」标记**（判据见 `stagedOnlyIds`），**不进出口选单**。
 * 逐读点的分类结果在 `lib/config-read-wiring.test.ts` 的 `MIRROR_SITES`。
 *
 * # 为什么重放到**镜像**上而不是取 `effectiveConfig.servers`
 *
 * `servers` 镜像恒等于 `config.servers`，两条路等价；但 `rules` 镜像**会与 `config.customRules` 分叉**
 * ——`RulesScreen` 的拖拽重排 / 开关切换是先写镜像的乐观更新（等一轮 IPC 会先弹回再跳过去）。
 * 取 `effectiveConfig.customRules` 会把那次乐观更新丢掉 ⇒ 拖完弹回原位，**且开关关着时也会发生**。
 * 重放到镜像上则两种情形都对：条目是「幂等的整体替换」，同一实体已被乐观写过再重放一次结果相同。
 *
 * # 引用稳定性是硬要求，不是优化（与 `effectiveConfigOf` 同一条）
 *
 * 本函数直接当 zustand selector 用：每次返回新数组 = 每次 store 变更都让整张列表重渲染，
 * 且 `useSyncExternalStore` 会因快照不稳定而抖动。故**每集合一个记忆化槽**，失效判据取入参引用
 * （镜像由 `set({servers})` 不可变替换、条目数组由 `stageEntry` 恒建新数组，引用即是判据）。
 *
 * 条目为空时早退返回镜像本体：`STAGED_CONFIG_ENABLED` 为 `false` 时条目恒空 ⇒ 本层不建任何对象、
 * 不占记忆化槽，在产品行为与渲染开销上都是逐字节零变化。
 * （注意：`replay` 自身对空条目集也是结构共享的 —— `{...baseline}` 换了外层对象，`.servers`
 * 仍是同一个数组。故早退**省的是那次分配**，引用稳定性两条路都成立。）
 */
interface CollectionMemo {
  mirror: unknown[] | null;
  entries: readonly StagedEntry[] | null;
  result: unknown[] | null;
}

const COLLECTION_MEMO: Record<'servers' | 'customRules', CollectionMemo> = {
  servers: { mirror: null, entries: null, result: null },
  customRules: { mirror: null, entries: null, result: null },
};

export function effectiveCollection<T>(
  collection: 'servers' | 'customRules',
  mirror: T[],
  entries: readonly StagedEntry[]
): T[] {
  if (entries.length === 0) return mirror;
  const memo = COLLECTION_MEMO[collection];
  if (mirror === memo.mirror && entries === memo.entries) return memo.result as T[];
  const next = (replay({ [collection]: mirror }, entries) as Record<string, unknown>)[collection];
  // `replay` 对畸形条目跳过而非抛；万一某条把集合换成了非数组，退回镜像而不是让列表崩掉。
  const result = Array.isArray(next) ? (next as T[]) : mirror;
  memo.mirror = mirror;
  memo.entries = entries;
  memo.result = result;
  return result;
}

/** 节点**列表**读点（展示面）。出口选单 / 切节点 / 按 id 下发 IPC 的读点仍读 `s.servers`。 */
export function useEffectiveServers(): ServerConfig[] {
  const entries = useStagedConfigStore((s) => s.entries);
  return useAppStore((s) => effectiveCollection('servers', s.servers, entries));
}

/** 规则**列表**读点（展示面）。乐观更新自身的基准（`getState().rules`）仍读镜像。 */
export function useEffectiveRules(): Rule[] {
  const entries = useStagedConfigStore((s) => s.entries);
  return useAppStore((s) => effectiveCollection('customRules', s.rules, entries));
}
