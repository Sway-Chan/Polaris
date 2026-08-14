/**
 * Home 屏（提取自原型 polaris-prototype.html L1633-1711）。
 *
 * 结构对齐原型：
 *  - .phead（标题 + meta：运行态 / 模式 / uptime）
 *  - .card.conn-card（统一连接控制卡 · 一卡两列）：
 *      左列 = 出口节点（trigger + 测速 + 连接/断开按钮）+ 解锁检测（ub 徽章 + 网络检测）
 *      右列 = 接管方式 seg2 + 分流策略 seg2
 *  - .card.pad.topo（流量拓扑 sankey：当前用 Top-N 条形 fallback 展示聚合连接，sankey SVG 后续批次补）
 *
 * 功能接 api-client（经 app-store）：
 *  - 连接/断开：startProxy / stopProxy（store 已封）
 *  - 模式切换：updateProxyMode（分流策略）/ config.proxyModeType（接管方式，写 config）
 *  - 解锁检测：unlockApi.run / onProgress / onUpdated / onInvalidated
 *  - 测速：「网络检测」按钮只测**当前选中出口**（见 `onSpeedTest`）；出口选单的「全部测速」才是全量
 *    入口（`onTestAllInMenu` 经 `speedTestableIds` 过滤）。结果读全局 `use-latency-store`
 *  - 拓扑：statsApi.subscribe('aggregate') + onConnectionsAggregate
 *  - 状态：proxyApi.getStatus / onStarted / onStopped / onError + ipInfoApi
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { useNavStore } from '@/store/nav-store';
import { api } from '@/ipc';
import { unlockApi, ipInfoApi } from '@/ipc/api-client';
import { toast } from '@/lib/error-handler';
import { cn } from '@/lib/utils';
import { relativeTimeText } from '@/lib/relative-time';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import {
  deriveConnectButtonState,
  connectButtonClass,
  deriveProxyPhase,
  type ConnectButtonAction,
} from './connect-button-state';
import { createTopicSubscription } from '@/lib/topic-subscription';
import { deriveTakeoverConnState } from './connection-state';
import { exitAddrText } from './exit-addr';
import { applyFakeIpTunEntry } from './fakeip-tun-entry';
import { notifyDesktop } from '@/lib/desktop-notify';
import { withProxyStartClaim } from '@/lib/proxy-start-claim';
import { ProxyErrorCode } from '@/contracts/types';
import type {
  ConnectionsAggregate,
} from '@/contracts/types';
import { ConnectionTopology } from './ConnectionTopology';
import { useSwitchNode } from '@/components/screens/shared/use-switch-node';
import { NodeMenu } from './NodeMenu';
import { TsExitWarning } from './TsExitWarning';
import { ReverseRoutingBadge } from './ReverseRoutingBadge';
import {
  BLOCK_SERVER_ID,
  DIRECT_SERVER_ID,
  isBlockSelection,
  isDirectSelection,
  isSentinelSelection,
} from '@/domain/direct-selection';
import { resolveExitNodeFlagCode } from '@/domain/exit-flag';
import { FlagImg } from '@/components/flag-img';
import { speedTestableIds } from '@/domain/endpoint-routes';
import { useLatencyStore } from '@/store/use-latency-store';
import { useSystemProxyLive } from '@/store/use-system-proxy-live';
import { isReverseRegionRouting } from '@/domain/region-routing';
import {
  ENABLED_SERVICE_IDS,
  type ServiceId,
  type UnlockStatus,
} from '@/contracts/unlock-detection';
import { brandIcon } from '@/components/brand-icons';
import { fmtUptime } from '../shared/format';
import {
  speedTestErrorMessage,
  notInPoolMessage,
  speedTestBlockedMessage,
} from '../shared/speedtest-feedback';
import { speedTestBlockReason } from '@/components/screens/nodes/nodes-logic';

/**
 * 手动网络检测冷却窗（置灰刷新钮）。与后端 force 硬下限 `FORCE_MIN_MS`
 * （`src-tauri/src/runtime/unlock.rs:83`，判定在 `:583` `force && last_at != 0 && now-last_at < FORCE_MIN_MS`）
 * **同值 15s**：本常量是前端的即时反馈（钮置灰 + tooltip），后端那道才是绕过 UI 也挡得住的硬下限。
 * 两者取值必须一致——前端更短会让用户点了却被后端静默吞成缓存返回。
 */
const UNLOCK_COOLDOWN_MS = 15_000;

/* ── 解锁检测服务展示名（对齐原型 UNLOCK_SVCS，分 AI / 流媒体两组）── */
interface UnlockSvcMeta {
  id: ServiceId;
  name: string;
  grp: 'ai' | 'stream';
  /**
   * 可选的**检测强度弱化说明** i18n key，附在 hover/aria 文案尾部。
   *
   * 仅给判据弱于其它服务的项用（当前只有 grok，且 grok 正停飞未上线）：它的 `ok` 只证明「站点可达 +
   * 未被风控拦截」，判不了登录后模型可用性（EU 的限制是模型级、站点仍返 200，裸 HTTP 不可见）。不写这一句，
   * 用户会把这颗绿点读成与 Netflix 绿点同等强度的结论——那是我们没测过的事。
   */
  hintKey?: string;
}
/** 已实现服务的展示元数据（含**未上线**项，与 `SERVICE_IDS` 同域）。渲染集见下方 `UNLOCK_SVCS`。 */
const UNLOCK_SVC_META: UnlockSvcMeta[] = [
  { id: 'chatgpt', name: 'ChatGPT', grp: 'ai' },
  { id: 'claude', name: 'Claude', grp: 'ai' },
  { id: 'gemini', name: 'Gemini', grp: 'ai' },
  // 停飞（`PENDING_CALIBRATION_SERVICE_IDS`）：弱检测 + G3 空规则 → 受限地区会被说成「已解锁」。
  // 元数据保留是为了「标定完成后翻一个开关就复现」，不是为了渲染一颗恒灰的装饰徽章。
  { id: 'grok', name: 'Grok', grp: 'ai', hintKey: 'home.unlockGrokWeakHint' },
  { id: 'netflix', name: 'Netflix', grp: 'stream' },
  { id: 'disney', name: 'Disney+', grp: 'stream' },
  { id: 'tiktok', name: 'TikTok', grp: 'stream' },
  { id: 'spotify', name: 'Spotify', grp: 'stream' },
];
/**
 * 实际渲染集 = 上线集（`ENABLED_SERVICE_IDS` 过滤，见 `contracts/unlock-detection.ts`）。
 * 停飞服务后端根本不探测（Rust `ServiceId::ALL` 不含它）→ 若照渲染只会得到一颗**永远 idle** 的灰点，
 * 用户读成「检测失败」；故这里直接不渲染，开关翻回来时它自己回来。
 */
const UNLOCK_SVCS: UnlockSvcMeta[] = UNLOCK_SVC_META.filter((s) =>
  ENABLED_SERVICE_IDS.includes(s.id)
);

/** 单服务徽章（原型 .ub）：状态点 + 品牌图标（随包本地 SVG，无图标回退服务名），checking 显琥珀脉冲。
 * 图标灰度随状态（timeout/blocked/idle 置灰，对齐原型 `.ub-ico` 处理）。
 *
 * `region` 是**该服务边缘节点判定的地区码**（`UnlockResult.region`，后端 `with_region()` 真填）——
 * 此前全链路死数据：类型有、后端填、渲染端零读。它回答的问题与状态点不同：状态说「能不能用」，
 * region 说「它把你当哪国用户」——同为 `ok`，US 与 JP 对用户的意义完全不同（能不能看到目标片库）。
 * 故并入 hover 文案，不另占版面。 */
function UnlockBadge({
  svc,
  status,
  region,
}: {
  svc: UnlockSvcMeta;
  status: UnlockStatus;
  region?: string;
}) {
  const { t } = useTranslation();
  const icon = brandIcon(svc.id);
  // 地区码统一大写（后端来源不保证大小写）；缺省时整段不拼，不留「· undefined」尾巴。
  const suffix = region ? ` · ${region.toUpperCase()}` : '';
  // 弱检测说明（当前仅 grok）：只在该服务真有 hintKey 时才拼，其余徽章文案一字不变。
  const hint = svc.hintKey ? ` · ${t(svc.hintKey)}` : '';
  // 状态是**机器枚举**（idle/checking/ok/partial/blocked/restricted/timeout），此前直接拼进
  // tooltip 与 aria-label ⇒ 无论界面语言是什么，悬停看到的恒为英文枚举名（2026-08-11 用户反馈）。
  // svc.name 是品牌名、region 是国家码，两者**刻意不翻**；只有 status 是给人读的。
  // 键集完整性由 `unlock-status-i18n.test.ts` 守（新增状态值而没同步五语种 → 红）。
  const statusText = t(`home.unlockStatus.${status}`);
  return (
    <span
      className={`ub ${status}`}
      aria-label={`${svc.name}: ${statusText}${suffix}${hint}`}
      data-tip={`${svc.name} · ${statusText}${suffix}${hint}`}
    >
      <span className="dot" />
      {icon ? <span className="ub-ico">{icon}</span> : svc.name}
    </span>
  );
}

/* ── 接管方式 / 分流策略 seg2 选项 ── */
type InterceptKind = 'systemProxy' | 'tun' | 'manual';
type RoutingKind = 'smart' | 'global' | 'direct';

const INTERCEPT_OPTS: { v: InterceptKind; labelKey: string }[] = [
  { v: 'systemProxy', labelKey: 'home.takeoverSystemProxy' },
  { v: 'tun', labelKey: 'home.takeoverTun' },
  { v: 'manual', labelKey: 'home.takeoverManual' },
];
const ROUTING_OPTS: { v: RoutingKind; labelKey: string }[] = [
  { v: 'smart', labelKey: 'home.routingSmart' },
  { v: 'global', labelKey: 'home.routingGlobal' },
  { v: 'direct', labelKey: 'home.routingDirect' },
];

/** phead meta 行的 mode-line 段（原型 #home-mode-line：接管方式 · 分流策略）。模式/策略文案复用
 * INTERCEPT_OPTS/ROUTING_OPTS 的 i18n key（单一真值源，避免与右列 seg2 各自硬编码导致改名/翻译时两处失步）。
 * runtime 段（#home-runtime/#home-uptime）结构不同（连接态时嵌套 uptime span，断开时纯文本），
 * 故拆到组件内联渲染，不并入本 hook（对齐原型 syncHome 的两处独立赋值）。 */
function useHomeModeLine(): string {
  const { t } = useTranslation();
  const config = useEffectiveConfig();
  return useMemo(() => {
    const interceptOpt = INTERCEPT_OPTS.find((o) => o.v === config?.proxyModeType);
    const mode = interceptOpt ? t(interceptOpt.labelKey) : '—';
    const routingOpt = ROUTING_OPTS.find((o) => o.v === config?.proxyMode);
    const strategy = routingOpt ? t(routingOpt.labelKey) : '';
    return strategy ? `${mode} · ${strategy}` : mode;
  }, [config, t]);
}

/**
 * 运行时长（秒）——由 `startTime` 在渲染端本地 tick 得出，**不读 `proxyStatus.uptime`**。
 *
 * `uptime` 是后端在 getStatus 应答那一刻现算的快照，而 proxyStatus 只在挂载 + proxyStarted/
 * proxyStopped 事件时刷新（App.tsx，无轮询）→ 直接渲染 uptime 会让时长冻在刷新那一刻不走字。
 * 未连接 / 无 startTime → undefined（fmtUptime 自行呈现缺省态），且此时不起定时器。
 */
function useUptimeSeconds(startTime: number | undefined, running: boolean): number | undefined {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!running || !startTime) return;
    setNow(Date.now()); // 立即对齐一次，避免首帧沿用上次停表时的 now
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [running, startTime]);
  if (!running || !startTime) return undefined;
  return Math.max(0, Math.floor((now - startTime) / 1000));
}

export function HomeScreen() {
  const { t } = useTranslation();
  const navigate = useNavStore((s) => s.navigate);
  const enterSettings = useNavStore((s) => s.enterSettings);

  /** 展示口径（分流策略高亮 / 反向地区分流徽章 / 订阅列表）：暂存的编辑要立刻回显。 */
  const config = useEffectiveConfig();
  /** 磁盘口径：下面两条 W-1/W-2 直落盘腿的**基准**。基准必须是盘，否则会把暂存的编辑一起写进去。 */
  const diskConfig = useAppStore((s) => s.config);
  const proxyStatus = useAppStore((s) => s.proxyStatus);
  const proxyStarting = useAppStore((s) => s.proxyStarting);
  const proxyStopping = useAppStore((s) => s.proxyStopping);
  const servers = useAppStore((s) => s.servers);
  const selectedServerId = useAppStore((s) => s.selectedServerId);
  const unlock = useAppStore((s) => s.unlock);
  /** 出口 IP 快照：与状态栏**同一份 store、同一次探测**（水合与订阅统一挂 App.tsx 顶层）。
   *  出口节点框的旗面只认它带回的 countryCode——绝不用节点名/入口域名派生（见 domain/exit-flag.ts）。 */
  const ipInfo = useAppStore((s) => s.ipInfo);

  const startProxy = useAppStore((s) => s.startProxy);
  const stopProxy = useAppStore((s) => s.stopProxy);
  // 起核 resolve 后回读真实状态，用于分辨「真连上了」与「让位/取消腿返 Ok 但核没起」。
  const refreshProxyStatus = useAppStore((s) => s.refreshProxyStatus);
  const setProxyStatus = useAppStore((s) => s.setProxyStatus);
  const setUnlock = useAppStore((s) => s.setUnlock);
  const beginUnlockCheck = useAppStore((s) => s.beginUnlockCheck);
  const applyUnlockSnapshot = useAppStore((s) => s.applyUnlockSnapshot);
  const saveConfig = useAppStore((s) => s.saveConfig);
  const switchNode = useSwitchNode();
  const openDialog = useDialogStore((s) => s.open);
  const closeDialog = useDialogStore((s) => s.close);
  const setServerPageAction = useAppStore((s) => s.setServerPageAction);

  const modeLine = useHomeModeLine();

  const [aggregate, setAggregate] = useState<ConnectionsAggregate | null>(null);
  const [busy, setBusy] = useState(false);
  /** 启停失败（本地错误态，喂三态圆钮的 err 分支）：store/后端均无代理错误流可订阅，见 onConnectToggle。 */
  const [connectError, setConnectError] = useState(false);
  /** 测速中：临时显示 spinner。 */
  const [testing, setTesting] = useState(false);
  /** 分流策略切换在飞（契约 L21 的 `routingBusy`）：置灰 seg2 + 单飞守卫，见 onRoutingChange。 */
  const [routingBusy, setRoutingBusy] = useState(false);
  /** 节点测速结果（供 NodeMenu 各行 + 当前出口共用；-1=后端判定不可测，键缺席=未测）。
   *  读**全局 store**（`use-latency-store`）：订阅挂 App.tsx 顶层，切屏不丢。勿改回组件私有 useState。 */
  const latencies = useLatencyStore((s) => s.latencyMap);
  /** 批量结果落库（invoke 返回值兜底同步，补事件丢失）。 */
  const applyLatencyResults = useLatencyStore((s) => s.applyLatencyResults);
  /** 网络检测冷却态（置灰刷新钮 + tooltip 说明）：见 UNLOCK_COOLDOWN_MS 的 DESIGN-REVIEW。 */
  const [unlockCooldown, setUnlockCooldown] = useState(false);
  /** 出口节点选单开合（原型 #node-menu，恢复内联下拉而非跳转节点页）。 */
  const [nodeMenuOpen, setNodeMenuOpen] = useState(false);
  const nodeDdRef = useRef<HTMLDivElement>(null);
  /** 本次解锁快照是否由用户点「网络检测」触发（喂 onUpdated 里的完成 toast，见 onUnlockRefresh）。 */
  const unlockUserRequested = useRef(false);
  /** 当前出口显示名。走 ref 而非直接读闭包：解锁事件订阅 effect 不该因换节点重挂（重挂会丢事件），
   *  但完成 toast 又要报出「经哪个出口」——ref 让订阅保持长命的同时读到最新名字。 */
  const exitNameRef = useRef('');

  /** **核在跑**——本页所有「能不能做这件事」的门都用它（解锁检测要核在跑才有出口可测、测速的
   *  `mainCorePool` 能力位、圆钮该显示断开动作、运行时长在走）。这些能力与系统代理设没设上无关。 */
  const connected = !!proxyStatus?.running;
  /** 系统代理**活态**（地面真相）：读**共享 store**，轮询驱动唯一挂在 App.tsx 顶层。
   *  与状态栏**同一份活态**——两处各起一份轮询会双倍 exec `networksetup`/`gsettings`/`reg`，
   *  且两条链不同相时会出现「首页说未生效、状态栏还亮绿灯」这种自相矛盾（本页的降级横幅与状态栏的
   *  状态点说的是同一件事，必须同源）。适用范围门与兜底口径收在 `store/use-system-proxy-live.ts`。 */
  const systemProxyLive = useSystemProxyLive();
  /** **展示口径**的连接态，按接管方式分叉（契约 L17）。与上面的 `connected` 刻意分开：
   *  systemProxy 没设上时核确实在跑（功能门该开），但「已连接」这句话是假的（流量在直连）——
   *  一个布尔量表达不了这两件事，合并回一个只会二选一地错。边界见 connection-state.ts。 */
  const connState = deriveTakeoverConnState({
    running: connected,
    proxyModeType: config?.proxyModeType,
    errorCode: proxyStatus?.errorCode,
    systemProxyLive,
  });
  /** 地区分流是否反向（回国）——喂「分流策略」标签行的持久指示器，见 ReverseRoutingBadge。 */
  const reverseRouting = isReverseRegionRouting(config);
  const uptimeSec = useUptimeSeconds(proxyStatus?.startTime, connected);
  const directSelected = isDirectSelection(selectedServerId);
  const blockSelected = isBlockSelection(selectedServerId);
  /** 出口是「非节点哨兵」（直连 / 阻断）：无节点承载，故 currentServer 恒 null 但**仍是有效出口**。 */
  const sentinelSelected = isSentinelSelection(selectedServerId);
  const currentServer = useMemo(() => {
    if (sentinelSelected) return null;
    const byId = servers.find((s) => s.id === selectedServerId);
    return byId ?? null;
  }, [servers, selectedServerId, sentinelSelected]);
  /** 空状态：一个节点都没有、且没选哨兵出口（直连/阻断都是有效出口配置，不算空）。 */
  const emptyState = servers.length === 0 && !sentinelSelected;
  /** 阻断在「直连模式」下无效：route.final 恒 = direct，没有流量经过 proxy-selector ⇒ 选了也是 no-op。
   *  返回禁用原因文案（null = 可选），交给选单渲染 disabled + tooltip，不留静默无效项。 */
  const blockDisabledReason = useMemo(
    () =>
      config?.proxyMode === 'direct'
        ? t('home.blockExitUnavailableInDirect')
        : null,
    [config?.proxyMode, t]
  );
  /** 空状态两条入口：**先落 action 再导航**——反过来会让节点页在 action 落库前就挂载完，
   *  一次性意图消费不到（消费在挂载 effect 里）。 */
  const goServerPage = useCallback(
    (action: 'add-server' | 'add-sub') => {
      setServerPageAction(action);
      navigate('nodes');
    },
    [setServerPageAction, navigate]
  );
  /** 出口旗面：与状态栏同一次探测，但**断开态一律留空**（本框旗面紧贴节点名/地址，画本机地区旗会被
   *  读成「这个节点在中国」——理由详见 domain/exit-flag.ts 的 resolveExitNodeFlagCode）。 */
  const exitFlagCode = resolveExitNodeFlagCode(connected, ipInfo?.proxy?.countryCode);

  useEffect(() => {
    exitNameRef.current = directSelected
      ? t('home.directConnection')
      : blockSelected
        ? t('home.routingBlock')
        : (currentServer?.name ?? '');
  }, [directSelected, blockSelected, currentServer?.name, t]);

  /* 设置页「在主页切换 →」此前会携一次性意图跳来、在「接管方式」分段控件上冒浮标（原型
   * `goto-home-intercept`, `proto:4052`）。**整条腿已按陈先生 2026-07-29 裁定移除** —— 与切节点那两枚
   * 浮标同一判断：跳转本身已经把用户送到目标屏，再冒一枚讲实现细节的浮标是噪声。
   * `HomePageAction` 通道与 `flash-hot` 模块随之删除（唯一消费方就是这里，留着即死代码）。 */

  /* ── 出口选单：点击外部 / Esc 关闭（对齐 NodesScreen 的 add-menu 模式）── */
  useEffect(() => {
    if (!nodeMenuOpen) return;
    const onDown = (e: MouseEvent) => {
      if (!nodeDdRef.current?.contains(e.target as Node)) setNodeMenuOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setNodeMenuOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [nodeMenuOpen]);

  /* ── 初始拉一次状态（App 已拉，这里仅兜底刷新，防 store 水合滞后）── */
  useEffect(() => {
    api.proxy
      .getStatus()
      .then((s) => setProxyStatus(s ?? null))
      .catch(() => {
        /* 非 Tauri 环境 mock undefined，忽略 */
      });
  }, [setProxyStatus]);

  /* ── 解锁检测「完成」toast ──
   *
   * **数据面（progress/updated/invalidated 写 store + 冷启动水合）已提到 App.tsx 顶层**——它必须跨页存活，
   * 挂在这里会随首页卸载而断（见 App.tsx 该 effect 的注释）。此处只留展示面：本组件是刷新钮所在地，
   * 「检测完成」toast 属该按钮的反馈，随首页卸载而静默是正确行为（用户已不在这个页面）。
   *
   * onUpdated 是**共享**终态流（切节点/起停/后台自跑都会推），故用 ref 门控：只有用户亲手点过刷新的那一轮
   * 才报，且无论如何都清标志（每轮只报一次），否则后台自动重检会平白弹 toast。 */
  useEffect(() => {
    return unlockApi.onUpdated(() => {
      if (!unlockUserRequested.current) return;
      unlockUserRequested.current = false;
      toast.success(
        t('home.unlockCheckDone', {
          node: exitNameRef.current,
        })
      );
    });
  }, [t]);

  /* ── 网络检测冷却（15s）：由 unlock.lastRunAt 派生，窗内置灰刷新钮。停代理（!connected）即灭
   * （钮本就 disabled，避免重连后陈旧 lastRunAt 误锁）。DESIGN-REVIEW 见 UNLOCK_COOLDOWN_MS。 */
  useEffect(() => {
    const lastRunAt = unlock.lastRunAt;
    if (!connected || lastRunAt == null) {
      setUnlockCooldown(false);
      return;
    }
    const remaining = UNLOCK_COOLDOWN_MS - (Date.now() - lastRunAt);
    if (remaining <= 0) {
      setUnlockCooldown(false);
      return;
    }
    setUnlockCooldown(true);
    const id = setTimeout(() => setUnlockCooldown(false), remaining);
    return () => clearTimeout(id);
  }, [connected, unlock.lastRunAt]);

  /* ── 拓扑聚合订阅：进 home 才订，离开退订（worker 逐级停机）──
   * 真机反馈：切到别的窗口再切回主页，拓扑显示滞后。根因：后端 aggregate poller 按 1s 定时轮询 +
   * 内容签名去重推送（不受窗口可见性门控——`should_stream` 门控逻辑在 stats-engine 定义了但从未接入
   * poller），故切走期间若拓扑内容有变化本会正常推送；用户感知的「滞后」是等不及下一轮自然推送。
   * 修法：窗口失焦 / 不可见时退订（省 gRPC 轮询），重新聚焦 / 可见时重订——重订会让后端重建 poller
   * （签名基线复位为空），首轮无条件判定「变了」强制推最新帧，等于一次主动拉取，而非空等下一次内容
   * 变化触发的推送。300ms 去抖：避免快速 alt-tab 把 poller 反复拆建。路由切回 home：ScreenRouter 按
   * mainScreen 整体卸载/重挂 HomeScreen，mount 时的首次订阅已天然覆盖，无需额外挂路由监听。 */
  useEffect(() => {
    let debounceId: ReturnType<typeof setTimeout> | null = null;

    /* 订阅生命周期交给 `createTopicSubscription`：它保证「监听先于订阅挂上」与「订过就一定退」
       两条不变式（各自守着一个真机缺陷，见该文件头注），并可脱离组件直测。 */
    const sub = createTopicSubscription<ConnectionsAggregate>(
      {
        onFrame: (cb) => api.stats.onConnectionsAggregate(cb),
        subscribe: () => api.stats.subscribe('aggregate'),
        unsubscribe: () => api.stats.unsubscribe('aggregate'),
      },
      setAggregate
    );
    sub.setWanted(true);

    /** 焦点/可见性变化去抖后同步订阅态：只有「可见且聚焦」才保持订阅。 */
    const sync = () => {
      if (debounceId) clearTimeout(debounceId);
      debounceId = setTimeout(() => {
        debounceId = null;
        sub.setWanted(document.visibilityState === 'visible' && document.hasFocus());
      }, 300);
    };

    window.addEventListener('focus', sync);
    window.addEventListener('blur', sync);
    document.addEventListener('visibilitychange', sync);

    return () => {
      if (debounceId) clearTimeout(debounceId);
      window.removeEventListener('focus', sync);
      window.removeEventListener('blur', sync);
      document.removeEventListener('visibilitychange', sync);
      sub.dispose();
    };
  }, []);

  /* ── 连接/断开按钮：startProxy / stopProxy（store 封装）──
   * 失败必须给用户反馈：此前只 console.error，UI 上启停失败与「什么都没发生」不可区分。
   * 错误态**本地持有**（store 无 proxyError 字段，EVENT_PROXY_ERROR 后端亦无 emit 点）：
   * 仅由本次 reject 置位，成功/重试即清 —— 不臆造后端错误流。 */
  const onConnectToggle = useCallback(async (action: ConnectButtonAction) => {
    if (action === 'none') return;
    // **取消腿**（启动中点击）：与 stop 走同一条后端通道，但**不**置 busy——本次点击不是新的一轮启停，
    // 而是打断在飞的那一轮；置 busy 会在 start 的 finally 里被抢着复位，反而把相位搅乱。
    // 相位由 store 的 proxyStopping 表达（stopping 压过 starting → 圆钮转「停止中」且不可重复点）。
    //
    // 后端半：`proxy_stop` 无前置状态守卫，`stop_inner` 先 bump 世代并**唤醒**在飞起核腿，等待
    // （退避 / 就绪轮询）就地中断 → 取消当场生效，而不是静默等这一轮走完（真机 ≈35s 的那一段）。
    if (action === 'cancel') {
      try {
        await stopProxy();
      } catch (err) {
        console.error('[home] cancel starting failed:', err);
        toast.error(
          t('home.stopProxyFailed'),
          err instanceof Error ? err.message : undefined
        );
      }
      return;
    }
    setBusy(true);
    // 重试即清旧错误：避免上一轮失败的橙态残留到本轮进行中
    setConnectError(false);
    try {
      if (action === 'stop') {
        await stopProxy();
      } else {
        // `withProxyStartClaim` 认领本次起核：提权门两码（HELPER_NOT_INSTALLED / HELPER_GATE_ABORTED）
        // 后端**双出口**（既 emit `event:proxyError`、又让本次 start reject），下方 catch 是发起方的
        // await 腿；不认领则 App.tsx 的事件腿会把同一次失败再报一遍（NOT_INSTALLED 更是 toast + 桌面通知
        // + 本文件的「去安装」模态三重）。认领期内事件腿让位，托盘/自动连接等无人 await 的入口不受影响
        // （它们不经此处 ⇒ 未认领 ⇒ 事件腿照常报）。**删掉这层包裹即回归双报**（有单测守卫）。
        await withProxyStartClaim(() => startProxy());
        // **resolve ≠ 连上了**：起核腿被取消/被更新的 start 接管时走「让位腿」，后端返 Ok（让位不是失败）
        // 但核并未运行。此处若无条件报「已连接」，用户点完取消反而收到一条连接成功提示 = 假报。
        // 拉一次真实状态再判——`running` 是唯一能诚实断言"连上了"的事实。
        await refreshProxyStatus();
        if (!useAppStore.getState().proxyStatus?.running) return;
        // 原型 :3430 连接成功即 notify('已连接 · 香港 IEPL·01','ok')——带出口名，用户一眼确认「连上的是哪个」。
        // 仅连接分支报（原型断开走 setConnected(false) 不 notify）。
        toast.success(
          t('home.connectedToast', {
            node: directSelected ? t('home.directConnection') : (currentServer?.name ?? ''),
          })
        );
      }
    } catch (err) {
      console.error('[home] connect toggle failed:', err);
      const code = (err as { code?: unknown } | null)?.code;
      // 用户在 TUN 提权引导门里点了「取消」——这是**用户达成的意图**，不是失败。橙色错误态 + 红 toast
      // 会把「我不想装」渲染成「出错了」。中性告知即可，且 `setConnectError` 提前于此判定之后（见下）。
      if (code === ProxyErrorCode.HELPER_GATE_ABORTED) {
        toast.info(t('errors.helperGateAborted'));
        return;
      }
      setConnectError(true);
      // helper 未安装 → 可操作对话框（去安装 → 设置›Helper），而非把裸 socket ENOENT 串塞进不可交互的
      // toast。toast 是 fire-and-forget（pointer-events:none + 2.2s 自动消失），承载不了「去安装」按钮。
      //
      // **本分支现在是引导门的 fallback，不再是主路径**：起核汇流点（`ProxyRuntime::start_inner`）已在
      // 弹框内就地授权安装并原地续起核，正常流程根本走不到这里。能走到 = 门里装失败 / 被非交互抑制 —— 此时
      // 「去设置页手动装」仍是正确的下一步，故保留。（此前它是主路径，用户点「安装」只会跳页 = 真机反馈 #2。）
      if (!connected && code === ProxyErrorCode.HELPER_NOT_INSTALLED) {
        useDialogStore.getState().open({
          kind: 'confirm',
          payload: {
            title: t('errors.helperNotInstalledTitle'),
            message: t('errors.helperNotInstalledDesc'),
            confirmLabel: t('errors.helperNotInstalledAction'),
            onConfirm: () => {
              useNavStore.getState().enterSettings('helper');
              useDialogStore.getState().close();
            },
          },
        });
        return;
      }
      // TUN 出口未夺到（其他 VPN 占默认路由）→ 专属可操作文案（非「启动失败」：核确起了、只是被硬闸拒标）。
      if (code === ProxyErrorCode.TUN_ROUTE_NOT_CAPTURED) {
        toast.error(
          t('errors.tunRouteNotCaptured'),
          err instanceof Error ? err.message : undefined
        );
        return;
      }
      // TUN 网卡从未建出来（#327）→ 专属可操作文案。**不能**落到下面的「sing-box 启动失败，请检查
      // 服务器配置」：wintun 驱动被拦/网卡建不出来与服务器配置毫无关系，照那句改永远修不好。
      // 也不能复用上一条：那条指向「断开其他 VPN」，对本现场同样是错的指引。
      if (code === ProxyErrorCode.TUN_ADAPTER_MISSING) {
        toast.error(
          t('errors.tunAdapterMissing'),
          err instanceof Error ? err.message : undefined
        );
        return;
      }
      // 残留 root 孤儿阻断起核 → 专属标题，**不能**落到下面的「sing-box 启动失败，请检查服务器
      // 配置」：那句话把用户导向错误的下一步（残留进程与服务器配置毫无关系，照它改永远修不好）。
      // 真正可操作的信息（该杀哪个 pid）在 err.message 里——Rust `escalate_root_orphans` 写死
      // 中文串带 pid，本码在 i18n 覆盖门里判断过后决定不配键（同义反复会把这句唯一诊断换掉），
      // 故仍以 err.message 作 description，见 contracts/proxy-error-key-coverage.test.ts 的 KEY_EXEMPT。
      if (code === ProxyErrorCode.ROOT_ORPHAN_BLOCKED) {
        toast.error(
          t('errors.rootOrphanBlocked'),
          err instanceof Error ? err.message : undefined
        );
        return;
      }
      toast.error(
        action === 'stop' ? t('home.stopProxyFailed') : t('errors.startupFailed'),
        err instanceof Error ? err.message : undefined
      );
    } finally {
      setBusy(false);
    }
  }, [connected, startProxy, stopProxy, refreshProxyStatus, directSelected, currentServer?.name, t]);

  /* ── 测速：**只测当前选中的出口/组网节点**（陈先生 2026-07-31 裁定，反向修 上游的一条）──
   *
   * # 为什么不再是全量（此前逐字对齐 上游 `connection-control-card.tsx:156` 的 `useSpeedTest(servers)`）
   *
   * 这颗「网络检测」按钮是三条腿的合并：延迟 + 解锁重检 + 出口 IP 重探。后两条走 `onUnlockRefresh`，
   * 它们**本来就只针对当前出口**（解锁结论以「经这个出口」为前提，出口 IP 更是只有一个）。延迟腿却
   * 测全部节点 ⇒ 同一次点击里两种射程，用户读不出这颗按钮到底在检测「谁」。
   *
   * 且这颗按钮自己的合并理由写着「出口选单里已有『全部测速』，这颗再只做延迟就是重复入口」——
   * 延迟腿若仍是全量，那个重复入口根本没消掉：两处点下去做的是**同一件事**（全量集合逐字相同）。
   * 全量入口保留在出口选单的 `onTestAllInMenu` 与节点页页头，那才是「全部测速」该在的地方。
   *
   * # 三种「测不了」必须各自出声，不许静默返回
   *
   * 断开态点它、且出口是哨兵/不可测节点时，`onUnlockRefresh` 那条腿也早退 ⇒ 整颗按钮零动作。
   * 此时不出声 = 与「按钮失灵」无从区分，故三条边界各给一句话：
   *  ① **哨兵出口（直连 / 阻断）**：`currentServer` 恒 null 但**仍是有效出口**（不是「没选」）。
   *     没有节点承载 ⇒ 结构上无延迟可测。跳过延迟腿并说明，不冒充成错误。
   *  ② **出口节点存在但结构上不可测**（reverseMesh / mesh-only / custom endpoint / TS 主核未就绪）：
   *     走 `speedTestBlockReason` 拿原因码 → `speedTestBlockedMessage` 出与节点页灰 ⚡ tooltip
   *     **逐字相同**的那句话。硬测只会产生 `-1`，而 `-1` 在 UI 上读作「真实超时」= 伪造数值。
   *  ③ **selectedServerId 指向已不存在的节点**：复用后端同条件的 `nodes.speedTestNoActiveExit`。
   *
   * # 单节点整轮静音这条不能破
   *
   * `total===1` 在 `speedtest-progress-toast.reduceSpeedTestProgress` 就被忽略（陈先生 2026-07-31 裁定：
   * 单节点不该弹「测速完成」）⇒ 改成单节点后本腿**成功路径零 toast**，反馈由按钮自身的 spinner
   * （`testing`）承担。`notInPoolMessage` 仍保留：那不是进度播报，是「这个节点根本没被测」的如实回报。 */
  const onSpeedTest = useCallback(async () => {
    if (sentinelSelected) {
      toast.info(
        t('nodes.speedTestSentinelExit')
      );
      return;
    }
    if (!currentServer) {
      toast.info(t('nodes.speedTestNoActiveExit'));
      return;
    }
    const reason = speedTestBlockReason(currentServer, { mainCorePool: connected });
    if (reason) {
      toast.info(speedTestBlockedMessage(reason, t));
      return;
    }
    setTesting(true);
    try {
      // 返回值兜底同步（事件丢失时补齐）+ 如实回报缺席节点（notInPool / tsNotReady）。
      const r = await api.server.speedTest([currentServer.id]);
      applyLatencyResults(r.results);
      const msg = notInPoolMessage(r, t);
      if (msg) toast.info(msg);
    } catch (err) {
      // 后端对「测不了」返失败信封（非静默空结果）→ 必须报，否则点了没反应与按钮失灵无从区分。
      console.error('[home] speedtest failed:', err);
      toast.error(speedTestErrorMessage(err, t));
    } finally {
      setTesting(false);
    }
  }, [sentinelSelected, currentServer, connected, applyLatencyResults, t]);

  /* ── 出口选单：拾取真实节点 ──
   * 切换本体（先判后切 / 差集走 pull / 节点名 await 前定格 / 两种 toast 互斥）已提到
   * `useSwitchNode`，与节点页卡片共用同一条腿 —— 两处各写一份就会分叉成「一个切得对、
   * 一个切得不对」。本地只保留「关掉选单」这一句属于本屏的收尾。
   *
   * 原型 `pickNode:3503` 在切换成功后冒 `flashHot('热切换 · 0 ms')` 浮标，**本仓刻意不移植**
   * （陈先生 2026-07-29 真机裁定：切节点已有 toast 报「切成了哪个」，再冒一枚讲实现细节的
   * 浮标是噪声——「热切换」是内核行为，用户不需要在每次切换时被告知）。反向修原型的一条。 */
  const onPickNode = useCallback(
    async (id: string) => {
      setNodeMenuOpen(false);
      await switchNode(id);
    },
    [switchNode]
  );

  /* ── 出口选单：拾取「直连」哨兵（DIRECT_SERVER_ID 写 selectedServerId，走 saveConfig 而非
   * switchServer —— server_switch 的 Rust 实现要求 id 命中真实 servers 列表，哨兵不在其中会被拒绝，
   * 而 saveConfig 走全量配置写入，config-engine 侧已对该哨兵放行校验，见 crates/store/validate.rs）── */
  const onPickDirectExit = useCallback(async () => {
    setNodeMenuOpen(false);
    if (!diskConfig) return;
    try {
      await saveConfig({ ...diskConfig, selectedServerId: DIRECT_SERVER_ID });
      // 同 onPickNode：选「直连」也是一次出口切换，原型 setNode :4439 一视同仁 notify。
      toast.success(
        t('home.switchedToast', {
          node: t('home.directConnection'),
        })
      );
      // 原型 `pickDirectExit:3514` 同样冒浮标，同 `onPickNode` 一并不移植（见那处理由）。
    } catch (err) {
      console.error('[home] switch to direct failed:', err);
      toast.error(t('home.switchError'));
    }
  }, [diskConfig, saveConfig, t]);

  /* ── 出口选单：拾取「阻断」哨兵。与 onPickDirectExit 同款走 saveConfig（server_switch 只收真实节点 id）。
   * 直连模式下该项在选单里已 disabled，此处二次守门：走到这里说明渲染态与配置态脱节（如刚被托盘改掉
   * proxyMode），静默返回胜过写入一个不会生效的出口。 */
  const onPickBlockExit = useCallback(async () => {
    setNodeMenuOpen(false);
    if (!diskConfig || blockDisabledReason) return;
    try {
      await saveConfig({ ...diskConfig, selectedServerId: BLOCK_SERVER_ID });
      toast.success(
        t('home.switchedToast', {
          node: t('home.routingBlock'),
        })
      );
    } catch (err) {
      console.error('[home] switch to block failed:', err);
      toast.error(t('home.switchError'));
    }
  }, [diskConfig, blockDisabledReason, saveConfig, t]);

  /* ── 出口选单「全部测速」（nm-foot menu-test-all）：测选单当前可见（含搜索过滤）的全部节点 ──
   *
   * 与首页圆钮 `onSpeedTest` **共用两样东西**，少任一样都会退回复审报的形态：
   *  ① 共用 `testing` 标志 —— 不共用则「菜单批量进行中，主测速按钮仍可点」：第二次请求撞后端单飞闸，
   *     返 `CODE_IN_FLIGHT` + 弹错误 toast。用户看见的是「测速失败」，实际只是自己撞了自己。
   *     反向同理，故进函数先看 `testing`。
   *  ② 共用 `speedTestableIds` 口径 —— 菜单给的是「当前可见（含搜索过滤）」的全部 id，其中可能含
   *     **结构上不可测**的节点（reverseMesh 走 OS default / custom endpoint 无 gate 真值 /
   *     TS-mesh-only 是公网黑洞）。它们必返 `-1`，而 `-1` 在 UI 上读作「真实超时」而非「未测」
   *     ⇒ 伪造数值。故先过 `isSpeedTestable` 再请求，与圆钮同口径（否则同一句「全部测速」两处不同义）。
   * 过滤后为空 → 提示而非空跑（同 `onSpeedTest`）。 */
  const onTestAllInMenu = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0 || testing) return;
      // 菜单只决定「测哪些**可见**节点」，不决定「哪些**结构上**可测」——后者是 speedTestableIds 的职责。
      const testable = new Set(speedTestableIds(servers, { mainCorePool: connected }));
      const target = ids.filter((id) => testable.has(id));
      if (target.length === 0) {
        toast.info(t('nodes.noTestableNodes'));
        return;
      }
      setTesting(true);
      try {
        // 如实回报缺席节点：未入运行核测速池的节点（订阅新增/改址后未重启核）本轮不会被测。
        const r = await api.server.speedTest(target);
        applyLatencyResults(r.results); // 返回值兜底同步（事件丢失时补齐）
        const msg = notInPoolMessage(r, t);
        if (msg) toast.info(msg);
      } catch (err) {
        console.error('[home] test all failed:', err);
        toast.error(speedTestErrorMessage(err, t));
      } finally {
        setTesting(false);
      }
    },
    [servers, connected, testing, applyLatencyResults, t]
  );

  /* ── 网络检测：解锁重检（force 绕 TTL）+ 出口 IP 强制重探（契约要求刷新钮同时驱动两者——
   * 解锁结论以「经当前出口」为前提，只重检解锁而留着陈旧出口 IP，两处结论会自相矛盾）。
   * 两者物理独立、失败互不牵连：出口重探失败不该吞掉解锁结果，故各自 catch。
   * 出口 IP 中间态/终值经 EVENT_IP_INFO_UPDATED 事件链回流 store，此处不回写返回值。 */
  const onUnlockRefresh = useCallback(async () => {
    if (!connected) return;
    beginUnlockCheck();
    // 本轮由用户亲手发起 → 放行 onUpdated 里的「网络检测完成」toast（原型 :3649）。
    unlockUserRequested.current = true;
    void ipInfoApi.get(true, true).catch((err) => {
      console.error('[home] exit ip reprobe failed:', err);
    });
    try {
      // **必须消费返回值**（对齐 上游 `use-unlock-detection.ts:85,97` 的 applyUnlockSnapshot）：
      // 后端有若干条 emit 了终态就早退的路径（gating 短路 / S-gate notReady / TTL 命中 / force 15s 硬下限），
      // 丢掉返回值时 `running:true` 就没人收口 —— 徽章一直转圈。这正是本批修的缺陷之一。
      applyUnlockSnapshot(await unlockApi.run(true));
    } catch (err) {
      // 失败时徽章从 checking 悄悄退回旧值，用户读作「检测了一下没变化」——实为根本没跑成。透出真实原因。
      console.error('[home] unlock run failed:', err);
      unlockUserRequested.current = false; // 没跑成 → 不许后续任何快照冒领这次的完成 toast
      setUnlock({ running: false });
      toast.error(
        t('home.unlockCheckFail'),
        err instanceof Error ? err.message : undefined
      );
    }
  }, [connected, beginUnlockCheck, applyUnlockSnapshot, setUnlock, t]);

  /* ── 网络检测（出口行那颗，合并自原「延迟测试」⚡ + 解锁区的 `.ub-detect`）──
   * 一次跑完三件事：延迟测速 / 解锁重检 / 出口 IP 强制重探。**三件事的射程统一 = 当前出口**
   * （2026-07-31 起延迟腿也收成单节点，见 `onSpeedTest` 上方；此前延迟腿是全量、与另两条不同义）。
   *
   * 两条腿**并发且互不牵连**（各自内部已 catch）：
   * - 延迟走 `onSpeedTest`，**不要求核在跑**（临时核腿独立于连接态）—— 合并不能把原 ⚡ 在断开态
   *   仍可测延迟这条能力弄丢；
   * - 解锁 + 出口 IP 走 `onUnlockRefresh`，它自带 `if (!connected) return`：这两项的结论以
   *   「经当前出口」为前提，核没跑就无出口可测，跑了只会得到假结论。
   * 故断开态点它 = 只测延迟，连接态点它 = 三件事全做。不另加分支，两个既有腿各自的门就是判据。 */
  const onNetworkCheck = useCallback(async () => {
    await Promise.allSettled([onSpeedTest(), onUnlockRefresh()]);
  }, [onSpeedTest, onUnlockRefresh]);

  /* ── 接管方式落盘：写 config.proxyModeType（需重启核才生效，由 configChanged 触发）。
   * 顺带消费「FakeIP-TUN 待纠正」快照：systemProxy 迁移冻结的 enableFakeIp:false 首次进 TUN 回 true。
   * 必须在 saveConfig **之前**对目标模式求值（flag 只在目标为 tun 时才消费），故先合成 next 再存。 */
  const applyIntercept = useCallback(
    async (v: InterceptKind) => {
      if (!diskConfig) return;
      try {
        const { config: next, corrected } = applyFakeIpTunEntry({
          ...diskConfig,
          proxyModeType: v,
        });
        await saveConfig(next);
        // 仅在真把 false 改回 true 时提示（罕见且有实际副作用需告知）；纯消费 flag 不打扰用户。
        // toast 在主窗可见时够用，但接管方式亦可从**托盘浮层**（独立窗口，FX-tray 已加入口）切换 →
        // 主窗 toast 此时看不到，故并发一条系统通知兜底（对齐 上游 notify-user；正文不含节点身份）。
        // 审计 diag-update：此前仅 toast.info，托盘触发的 FakeIP 自动启用会被完全漏掉。
        if (corrected) {
          toast.info(t('settings.proxyMode.fakeIpAutoEnabled'));
          void notifyDesktop(
            t('notify.fakeIpTun.title'),
            t('notify.fakeIpTun.body')
          );
        }
      } catch (err) {
        console.error('[home] set intercept failed:', err);
        toast.error(t('settings.proxyMode.failUpdate'));
      }
    },
    [diskConfig, saveConfig, t]
  );

  /* ── 接管方式切换入口：已连接时切换会重启核、瞬断当前连接 → 先弹确认；未连接直接落盘。 */
  const onInterceptChange = useCallback(
    (v: InterceptKind) => {
      if (!config || v === config.proxyModeType) return;
      if (!connected) {
        void applyIntercept(v);
        return;
      }
      openDialog({
        kind: 'confirm',
        payload: {
          title: t('settings.proxyMode.confirmTitle'),
          message: t('settings.proxyMode.confirmDesc'),
          confirmLabel: t('settings.proxyMode.confirmBtn'),
          onConfirm: () => {
            closeDialog(); // 回调自行 pop（dialog-store 不自动关）
            void applyIntercept(v);
          },
        },
      });
    },
    [config, connected, applyIntercept, openDialog, closeDialog, t]
  );

  /* ── 分流策略切换：updateProxyMode（store 热切换，不重启）── */
  const onRoutingChange = useCallback(
    async (v: RoutingKind) => {
      // 契约 L21 的本地 `routingBusy` 反馈。缺它时 seg2 在飞期间照样可点：`updateProxyMode` 是
      // 「invoke → 回来才写 store」，按钮高亮直到应答才移动，用户读作「没点上」→ 连点 → 多条
      // config:updateMode 并发写同一份配置，最后一条落盘的未必是最后点的那档（末次写覆盖）。
      // 单飞守卫 + 置灰是同一件事的两半：守卫保正确性，置灰把「正在生效」说出来。
      if (routingBusy) return;
      setRoutingBusy(true);
      try {
        await useAppStore.getState().updateProxyMode(v);
      } catch (err) {
        // seg2 是受控的（跟 config 走）：写失败即按钮弹回原档，用户读作「点不动」而非「没存上」。
        // 分流策略切换成功不报（seg2 高亮位移即反馈，原型同样只在「切回智能」那条快捷路径上 notify）。
        console.error('[home] set routing failed:', err);
        toast.error(
          t('rules.saveFailed'),
          err instanceof Error ? err.message : undefined
        );
      } finally {
        setRoutingBusy(false);
      }
    },
    [routingBusy, t]
  );

  /* ── 连接圆钮三态（契约「三态圆钮」）：派生收口到 deriveConnectButtonState，组件只做态→类/文案映射。
   * 「已配置」含 direct/block 两个哨兵出口：它们都不需要节点承载也能起核（config-engine 侧已豁免
   * selectedServer 存在性校验），故不可只看 currentServer——否则选了阻断后圆钮变灰、用户无法启动，
   * 而且界面会同时谎报成「请配置服务器」。 */
  const serverConfigured = sentinelSelected || !!currentServer?.id;
  // busy = 本地在飞标志，方向由当前连接态定（连着→停止中，未连→启动中）；store 相位优先。
  // 归一收口到 `deriveProxyPhase`（与托盘同一口径）：**stopping 压过 starting** —— 取消一次启动时
  // 两个标志同时为真（start 还在飞、stop 已发出），starting 若优先，圆钮会在取消途中仍显"可点取消"，
  // 用户每多点一次就多发一条 stop。
  const proxyPhase = deriveProxyPhase({
    starting: proxyStarting || (busy && !connected),
    stopping: proxyStopping || (busy && connected),
  });
  const connBtn = deriveConnectButtonState({
    proxyPhase,
    isConnected: connected,
    hasError: connectError,
    isServerConfigured: serverConfigured,
  });
  const btnClass = connectButtonClass(connBtn.kind);
  const btnTitle = connBtn.busy
    ? // 启停两相位此前共用「断开中...」，连接过程中显此文案与实际动作相反；派生态已能区分。
      // starting 现在是**可点的取消入口**，文案必须说出这件事——只写「连接中」用户读作"等着吧"，
      // 而这恰恰是真机事故里他找不到出口的那 35s。
      connBtn.kind === 'starting'
      ? t('home.cancelStarting')
      : t('home.disconnecting')
    : !serverConfigured
      ? t('home.plsConfigServer') // disabled 须给原因，否则用户不知为何点不动
      : connBtn.kind === 'error'
        ? t('home.connectErrorClickToRetry')
        : connected
          ? t('home.connectedClickToDisconnect')
          : t('home.disconnectedClickToConnect');

  // 解锁分组徽章渲染
  const renderUnlockGroup = (grp: 'ai' | 'stream') => {
    const svcs = UNLOCK_SVCS.filter((s) => s.grp === grp);
    return svcs.map((svc) => {
      const r = unlock.results[svc.id];
      const status: UnlockStatus = unlock.running
        ? 'checking'
        : r?.status ?? 'idle';
      return (
        // 检测中不显上一轮的 region：那是**旧结论**的地区，配在「检测中」旁边会被读成本轮已测出。
        <UnlockBadge
          key={svc.id}
          svc={svc}
          status={status}
          region={unlock.running ? undefined : r?.region}
        />
      );
    });
  };

  // 根因：曾用 !connected 当"仍在拉取初始状态"的替身——但核未运行时 connected 恒为 false，
  // 一旦 getStatus() 落定（proxyStatus 从 null 变为 { running:false }），!connected 依旧为真，
  // 导致「正在获取状态...」永久卡死（不是等待，是终态被误判成加载中）。改用 proxyStatus===null
  // （尚未收到过一次状态）才判定"加载中"；一旦有值，未连接就直接收敛到「—」终态。
  const detectTsText = proxyStatus === null
    ? t('home.fetchingStatus')
    : !connected
      ? '—'
      : unlock.running
        ? '...'
        : unlock.checkedAt
          ? // 契约要求**相对时间**（「3 小时前」），不是绝对时钟。绝对时刻要用户自己拿当前时间去减
            // 才知道新旧，而这一栏的全部用途就是回答「这个结论还新鲜吗」。档位与节点页订阅栏的
            // 「上次更新」共用同一份实现（`lib/relative-time.ts`），阈值不会两边分叉。
            relativeTimeText(unlock.checkedAt, t)
          : '—';

  return (
    <section className="screen" id="s-home">
      <div className="phead">
        <div>
          <h1 tabIndex={-1}>{t('home.pageTitle')}</h1>
        </div>
        <div className="meta" id="home-meta">
          <span id="home-mode-line">{modeLine}</span> ·{' '}
          <span id="home-runtime">
            {connected ? (
              <>
                <span>{t('home.runningPrefix')}</span>{' '}
                <span id="home-uptime">{fmtUptime(uptimeSec)}</span>
              </>
            ) : (
              t('home.stopped')
            )}
          </span>
          {/* 系统代理没设上（核在跑但流量在直连）→ 就地说破。运行时长照显（核确实在跑），但必须紧跟一条
              「未生效」——否则这一行读起来就是「系统 · 智能 · 运行中 12:34」，与真相相反。 */}
          {connState === 'proxy-degraded' && (
            <>
              {' · '}
              <span className="home-degraded" data-tip={t('home.statusProxyDegradedHint')}>
                {t('home.statusProxyDegraded')}
              </span>
            </>
          )}
        </div>
      </div>

      {/* 降级横幅：meta 行那句是「一眼扫到」，这里给可操作的解释（去哪查、怎么恢复）。
          仅 systemProxy 降级时出现；TUN/manual 不存在这条腿（见 connection-state.ts）。 */}
      {/* 色阶 `--warn`（`.pending-bar` 基线色）而非 `.err` 红：降级不是错误 —— 核在跑、只是流量没经核。
          同语义的另两处（状态栏 `.dot.warn` / meta 行 `.home-degraded`）都是琥珀，这里留红会让
          **同一屏上**同一件事出现两种色阶，用户读作两个严重程度（2026-07-28 复审 LOW #7）。 */}
      {connState === 'proxy-degraded' && (
        <div className="pending-bar" role="alert">
          <span className="pb-ic" aria-hidden>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M10.3 3.9 2.4 18a1.9 1.9 0 0 0 1.7 2.9h15.8A1.9 1.9 0 0 0 21.6 18L13.7 3.9a1.9 1.9 0 0 0-3.4 0Z" strokeLinejoin="round" />
              <path d="M12 9v4" strokeLinecap="round" />
              <circle cx="12" cy="16.6" r="0.9" fill="currentColor" stroke="none" />
            </svg>
          </span>
          {/* `pb-static`：这条只是告知，没有可点的东西 —— 不继承 `.pb-tx` 的 `cursor:pointer`（假可点）。 */}
          <div className="pb-tx pb-static">
            <b>{t('home.statusProxyDegraded')}</b>
            <div>{t('home.statusProxyDegradedHint')}</div>
          </div>
        </div>
      )}

      {/* 统一连接控制卡（一卡两列） */}
      <div className="card conn-card">
        <div className="cc-cols">
          {/* 左列：出口节点 + 解锁检测 */}
          <div className="cc-col left">
            {/* 出口节点 */}
            <div className="cc-field">
              <div className="field-lbl">
                <span>{t('home.exitNode')}</span>
                <span className="detect-ts" id="unlock-ts" data-tip={t('home.unlockCheckTimeHint')}>
                  {detectTsText}
                </span>
              </div>
              <div className="exit-row">
                {/* 空状态（契约「主页 Home · 空状态」）：一个节点都没有时，出口下拉里除了「直连」什么也
                    选不出来，摆着它等于让用户对着空菜单找出路。换成两条直达入口，并**携 action** 跳节点页
                    ——到站即弹对应对话框，意图不在导航途中丢掉（机制见 app-store 的 ServerPageAction）。
                    `directSelected` 时不进空态：用户已明确选了直连出口，那是有效配置而非「还没配」。 */}
                {emptyState ? (
                  <div className="home-empty-exit">
                    <span className="he-tx">{t('home.noServerConfig')}</span>
                    <button
                      type="button"
                      className="btn ghost sm"
                      onClick={() => goServerPage('add-server')}
                    >
                      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9">
                        <path d="M12 5v14M5 12h14" strokeLinecap="round" />
                      </svg>
                      {t('home.addServer')}
                    </button>
                    <button
                      type="button"
                      className="btn ghost sm"
                      onClick={() => goServerPage('add-sub')}
                    >
                      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9">
                        <path d="M4 11a9 9 0 019 9M4 4a16 16 0 0116 16" strokeLinecap="round" />
                        <circle cx="5" cy="19" r="1.4" fill="currentColor" stroke="none" />
                      </svg>
                      {t('home.addSubscription')}
                    </button>
                  </div>
                ) : (
                <div className={cn('node-dd', nodeMenuOpen && 'open')} id="home-node-dd" ref={nodeDdRef}>
                  <button
                    className="node-trigger"
                    id="node-trigger"
                    onClick={() => setNodeMenuOpen((v) => !v)}
                    aria-haspopup="listbox"
                    aria-expanded={nodeMenuOpen}
                    aria-controls="node-menu"
                    data-tip={t('home.switchNodeTip')}
                    aria-label={t('home.switchNodeTip')}
                  >
                    {/* 出口旗面：**出口 IP 探测回来的 countryCode**，与状态栏同源同一次探测。
                        绝不用 currentServer.name/address 派生 —— 那是**入口**，中转链下真实落地可能在别处，
                        用入口冒充出口比不画旗更糟（详见 domain/exit-flag.ts）。**未连接同样不画**：本槽位
                        紧邻下面的节点名与地址，断开态画本机地区旗会渲染成「HK03 / hk03.x.com:443 🇨🇳」，
                        被读成「这个节点在中国」——同一类冒充。未连接 / 未探到 → 整个槽位不渲染
                        （`.exit-flag` 是 flex 子项，不留则不占位），也不画地球：地球会被读成「出口在某个
                        未知国家」，而真相是「还没探到」。 */}
                    {exitFlagCode && (
                      <span className="exit-flag" id="exit-flag">
                        <FlagImg code={exitFlagCode} />
                      </span>
                    )}
                    <div className="exit-info">
                      <div className="exit-name">
                        {/* 动作标签轴：与状态栏那格同一状态的两个视图，色必须同源（详见 StatusBar 同段注释）。 */}
                        <span id="cur-node" className={blockSelected ? 'act-block-txt' : undefined}>
                          {directSelected
                            ? t('home.routingDirect')
                            : blockSelected
                              ? t('home.routingBlock')
                              : currentServer?.name ?? t('home.plsConfigServer')}
                        </span>
                        {!sentinelSelected && currentServer?.protocol && (
                          <span className="pill proto" id="cur-proto">
                            {currentServer.protocol.toUpperCase()}
                          </span>
                        )}
                      </div>
                      <div className="exit-addr" id="cur-addr">
                        {directSelected
                          ? t('home.directExitAddr')
                          : blockSelected
                            ? t('home.blockExitAddr')
                            : (exitAddrText(currentServer) ?? '—')}
                      </div>
                    </div>
                    <svg className="nt-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                      <path d="M7 10l5 5 5-5" />
                    </svg>
                  </button>
                  <NodeMenu
                    open={nodeMenuOpen}
                    servers={servers}
                    subscriptions={config?.subscriptions ?? []}
                    selectedServerId={selectedServerId}
                    latencies={latencies}
                    onPick={onPickNode}
                    onPickDirect={onPickDirectExit}
                    onPickBlock={onPickBlockExit}
                    blockDisabledReason={blockDisabledReason}
                    onTestAll={onTestAllInMenu}
                    onManage={() => {
                      setNodeMenuOpen(false);
                      navigate('nodes');
                    }}
                  />
                </div>
                )}
                {/* 网络检测（原「延迟测试」⚡）：出口选单里已有「全部测速」，这颗再只做延迟就是重复入口。
                    改为一次跑完三件事 —— 延迟 + 解锁重检 + 出口 IP 强制重探（陈先生 2026-07-30 裁定），
                    原先分散在解锁区那颗 `.ub-detect` 上的能力整体并过来，那颗随之删除。
                    图标同步换成雷达（原 ⚡ 只表达「测速」，撑不起合并后的语义）。 */}
                <button
                  className="icon-btn"
                  onClick={() => void onNetworkCheck()}
                  disabled={testing || unlockCooldown}
                  data-tip={unlockCooldown ? t('home.unlockCooldown') : t('home.networkCheckTip')}
                  aria-label={unlockCooldown ? t('home.unlockCooldown') : t('home.networkCheckTip')}
                >
                  {testing ? (
                    <span className="spinner" />
                  ) : (
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                      <circle cx="12" cy="12" r="7.5" />
                      <circle cx="12" cy="12" r="2.5" />
                      <path d="M12 2v2.5M12 19.5V22M2 12h2.5M19.5 12H22" />
                    </svg>
                  )}
                </button>
                <button
                  className={`connect-btn ${btnClass}`}
                  id="connect-btn"
                  onClick={() => void onConnectToggle(connBtn.action)}
                  disabled={connBtn.disabled}
                  data-tip={btnTitle}
                  aria-label={btnTitle}
                  aria-pressed={connected}
                >
                  {connBtn.kind === 'error' ? (
                    // err 态换「!」图标：仅靠底色区分 err/busy（同为暖色）对色觉障碍用户不可辨
                    <svg id="connect-ic" viewBox="0 0 24 24" fill="none">
                      <path d="M12 6v7" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                      <circle cx="12" cy="17" r="1.15" fill="currentColor" />
                    </svg>
                  ) : (
                    <svg id="connect-ic" viewBox="0 0 24 24" fill="none">
                      <path d="M12 3v9" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
                      <path d="M6.5 6.8a7 7 0 108.9 0" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
                    </svg>
                  )}
                </button>
              </div>
              {/* Tailscale 出口名不副实警示（选中 TS 当出口却出不了公网 → 行内注脚 + 直达 TS 设置）。 */}
              <TsExitWarning />
            </div>

            <div className="cc-hair" />

            {/* 解锁检测 */}
            <div className="cc-field unlock-field">
              <div className="unlock-row" id="unlock-row" style={{ opacity: connected ? 1 : 0.45 }}>
                {/* 两组徽章合并成**一行**（陈先生 2026-07-30 裁定）：原本 AI / 流媒体 各占一行，
                    连同两个 `.ul-lbl` 文字标签一共吃掉首页卡片一整行高度，而那行给连接拓扑更值。
                    分组语义不靠文字标签承载了 —— 用一个 `·` 分隔 + 每组一个 `role="group"`
                    的 `aria-label`：视觉上省掉两个词，读屏拿到的分组信息**一点没少**。
                    （把标签直接删掉而不补 aria 才是回归：徽章图标对读屏用户本就只有服务名。） */}
                <div className="unlock-line">
                  {/* 「AI」不进 i18n：它在五种语言里都是同一个词，为它开一条键只会让五份 locale
                      各存一份 'AI'。流媒体那组有既有键，照用。 */}
                  <span role="group" aria-label="AI" className="ub-grp">
                    {renderUnlockGroup('ai')}
                  </span>
                  {/* 两组之间的分隔：**空元素 + CSS 画的短虚线**（`.ub-sep`，见 styles/index.css）。
                      刻意不放字符 —— 字形推进随字体族浮动，会把徽章行的单行预算搞成一个随引擎变的量。 */}
                  <span className="ub-sep" aria-hidden />
                  <span
                    role="group"
                    aria-label={t('home.unlockGroupStreaming')}
                    className="ub-grp"
                  >
                    {renderUnlockGroup('stream')}
                  </span>
                </div>
              </div>
              {/* 原 `.ub-detect`「网络检测」按钮已删（陈先生 2026-07-30 裁定：与出口行那颗合并）。
                  它的三道防连点闸门随之由合并后那颗承担，语义一字未改：
                  ①前端 15s 冷却（`unlockCooldown`，由 lastRunAt 派生）；②后端 `run_lock` 单飞串行；
                  ③后端 `FORCE_MIN_MS` 15s 硬下限（脚本绕过前端也挡得住）。
                  **仍不因 `unlock.running` 禁用**：那是「检测卡在检测中」时用户唯一的自救入口，
                  用 running 锁它等于把死锁焊死（真机反馈过的一条）。 */}
            </div>
          </div>

          {/* 右列：接管方式 + 分流策略 */}
          <div className="cc-col right">
            <div className="cc-field">
              <div className="field-lbl">
                <span>{t('home.takeoverMethod')}</span>
                <button
                  className="seg-gear"
                  onClick={() => enterSettings('tun')}
                  data-tip={t('home.tunSettingsTip')}
                  aria-label={t('home.tunSettingsTip')}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <circle cx="12" cy="12" r="3" />
                    <path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3" />
                  </svg>
                </button>
              </div>
              <div className="seg-wrap">
                <div
                  className="seg2"
                  role="group"
                  aria-label={t('home.takeoverMethod')}
                >
                  {INTERCEPT_OPTS.map((o) => (
                    <button
                      key={o.v}
                      className={config?.proxyModeType === o.v ? 'on' : ''}
                      onClick={() => onInterceptChange(o.v)}
                    >
                      {t(o.labelKey)}
                    </button>
                  ))}
                </div>
              </div>
            </div>
            <div className="cc-field">
              <div className="field-lbl">
                <span>{t('home.routingStrategy')}</span>
                <ReverseRoutingBadge reverse={reverseRouting} />
                <button
                  className="seg-gear"
                  onClick={() => enterSettings('network')}
                  data-tip={t('home.networkSettingsTip')}
                  aria-label={t('home.networkSettingsTip')}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <circle cx="12" cy="12" r="3" />
                    <path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3" />
                  </svg>
                </button>
              </div>
              <div className="seg-wrap">
                <div
                  className="seg2"
                  role="group"
                  aria-label={t('home.routingStrategy')}
                  aria-busy={routingBusy}
                >
                  {ROUTING_OPTS.map((o) => (
                    <button
                      key={o.v}
                      className={config?.proxyMode === o.v ? 'on' : ''}
                      // 在飞期间整组置灰：反馈「正在生效」+ 兜住连点（守卫在 onRoutingChange 里，
                      // 因为托盘等其它入口不经这组按钮）。
                      disabled={routingBusy}
                      onClick={() => onRoutingChange(o.v)}
                    >
                      {t(o.labelKey)}
                    </button>
                  ))}
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* 流量拓扑（Sankey：几何取原型、缩放取 issue #303 定稿，见 topology-layout.ts） */}
      {/* 断开态 stub 不带 CTA：本页顶部圆钮（`onConnectToggle`）已是同一动作的唯一入口。 */}
      <ConnectionTopology aggregate={aggregate} disconnected={!connected} />
    </section>
  );
}

export default HomeScreen;
