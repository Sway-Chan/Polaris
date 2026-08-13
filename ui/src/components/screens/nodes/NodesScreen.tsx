/**
 * NodesScreen —— 节点屏（1:1 提取自原型 polaris-prototype.html L1737-1868 #s-nodes
 * + renderMesh/meshItem L4816-4842/4806-4815 组网入口 + syncNodeToolbar L4482-4485 工具栏行为）。
 *
 * 原型 DOM（class/层级对齐，样式见 src/styles/screens.css L「NODES」段 + components.css 通用类）：
 *   .screen
 *     .phead（.ph-title[h1 + .nd-count] + .acts：全部测速 + 添加 dropdown）
 *     .nd-tabs-scroll#node-tabs-scroll > .sub-tabs[data-tabgroup]（自建 / 组网 / 各订阅）
 *     .nd-subinfo（订阅 .sub-info，随对应 tab 显隐）
 *     .node-toolbar#node-shared-tools（.seg2 视图 + .input.search-box 搜索 + .sel 协议/排序（方向固定，无 .nh-dir）+ 测速（可见集）+ 多选）
 *       —— 多选按钮（.nt-hide-sub）仅在订阅 tab 隐藏（原型 syncNodeToolbar：isSub && 隐藏 + 自动退出批选）
 *     .batch-bar（多选批量操作条）
 *     .node-grid > .nd-card（组网协议从页头统一「添加」菜单进入，不在列表区重复铺入口）
 *       —— 各 tab pane
 *
 * 数据流：useAppStore（config.servers + config.subscriptions + selectedServerId）。
 * 测速经 api.server.speedTest 发起；延迟结果读全局 `use-latency-store`、进度走全局 sticky toast
 * （`lib/speedtest-progress-toast.ts`），两者的订阅都挂 App.tsx 顶层、切屏不丢。
 * **本屏不再自订 onSpeedTestProgress**，也不再画屏内进度行——见 `runSpeedTest` 上方的判据。
 */

import { useEffect, useMemo, useRef, useState, useCallback } from 'react';
import { useAppStore, useEffectiveConfig, useEffectiveServers } from '@/store/app-store';
import { useNodeSortStore } from '@/store/use-node-sort-store';
import { useLatencyStore } from '@/store/use-latency-store';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import { toast } from '@/lib/error-handler';
import {
  speedTestErrorMessage,
  notInPoolMessage,
  speedTestBlockedMessage,
} from '../shared/speedtest-feedback';
import type { SpeedTestInvokeResult } from '@/contracts/speed-test';
import { Csel } from '@/components/dialogs/Csel';
import { api } from '@/ipc';
import type { ServerConfig, SubscriptionConfig } from '@/contracts/types';
import { groupServersBySubscription } from '@/domain/server-grouping';
import { initialNodesTab } from './initial-tab';
import {
  collectRuleTargetedServerIds,
  isMeshNode,
  meshAllowsInternet,
  meshForceRoutedServers,
  meshShadowedCidrs,
  meshSingletonConflict,
  speedTestableIds,
  type SpeedTestCaps,
} from '@/domain/endpoint-routes';
import { sortServersByLatency } from '@/domain/server-latency-sort';
import { refreshSubscriptionWithToast } from '@/domain/subscription-refresh';
import { useSubscriptionProgress } from '@/store/use-subscription-progress-store';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { NodeCard } from './NodeCard';
import { SubInfoBar } from './SubInfoBar';
import { NdFlagDefs } from './nd-flag';
import { editDialogFor } from './node-edit-routing';
import { fallbackExitAfterDelete } from './node-delete-fallback';
import { useNavStore } from '@/store/nav-store';
import { useNodeViewStore } from '@/store/use-node-view-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute, splitStagedOnly, stagedOnlyIds } from '@/lib/staged-config';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { useSwitchNode } from '@/components/screens/shared/use-switch-node';
import { willRestartOnSelect } from '@/components/screens/home/pending-select-hint';
import { useAnchoredMenu } from '@/lib/use-anchored-menu';
import {
  speedTestIdsForSelection,
  speedTestBlockReason,
  type SpeedTestBlockReason,
  invalidNodeIndex,
  canMoveToGroup,
  subDeleteNodeCount,
  type SubMenuItem,
  nodeUseAction,
  type NodeUseVia,
} from './nodes-logic';

type SortKey = 'default' | 'name' | 'lat' | 'proto';

/** 批量删除按钮的原地二次确认 key（单节点删除用 `node-del:<id>`，见 requestDelete）。 */
const BATCH_DEL_KEY = 'batch-del';

export function NodesScreen() {
  const { t } = useTranslation();
  const config = useEffectiveConfig();
  /** 展示面：节点列表本体 —— 「节点列表不回显 staged 编辑」那条缺口在本屏的落点。 */
  const servers = useEffectiveServers();
  /** 操作面：磁盘上真实存在的那批节点。**只**用来算「哪些是 staged-only」（见 stagedOnlyIds），
   *  不参与渲染集合本身。 */
  const diskServers = useAppStore((s) => s.servers);
  /** 「待保存」角标的唯一判据：在 effective 里、不在 disk 里。不新造字段、不新造词汇。 */
  const stagedOnly = useMemo(() => stagedOnlyIds(servers, diskServers), [servers, diskServers]);
  const selectedServerId = useAppStore((s) => s.selectedServerId);
  const openDialog = useDialogStore((s) => s.open);
  const closeDialog = useDialogStore((s) => s.close);
  /**
   * 删节点 / 批删的原地二次确认（原型 :4140 `node-del`、:4137 `batch-del` 都走 confirmTwice）。
   * 与本屏另外两处**保留弹窗**的破坏性操作（删订阅 `requestSubDelete`、注销 WARP `removeWarpNode`）
   * 分工明确：那两处原型里没有对应的 confirmTwice 调用点，属本仓自加的确认，形态维持现状。
   */
  const { armed: confirmArmed, confirmTwice } = useConfirmTwice();
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  /** 撤销腿的两个入参（`ENTITY_ACTION_TABLE` 的 `revert` 策略）。总开关关着时 entries 恒空、
   *  `stagedOnly` 恒空 ⇒ 下面三条动作腿都走今天那条路径。 */
  const stagedEntries = useStagedConfigStore((s) => s.entries);
  const revertStaged = useStagedConfigStore((s) => s.revert);

  const subscriptions = config?.subscriptions ?? [];

  // 「添加 ▾」下拉菜单（原型 nodesAddMenu :3750：手动添加 / 手动导入 / 添加订阅）。
  const [addMenu, setAddMenu] = useState(false);
  const addWrapRef = useRef<HTMLDivElement>(null);
  /* 三处工具栏/网卡下拉菜单的定位与首项聚焦收口到 `useAnchoredMenu`（原型 miniMenu :3245-3253）：
     此前是纯 CSS 锚定（`top:calc(100% + 6px)` + `left/right:0`），零测量零 clamp ⇒ 窄窗时出屏。 */
  const addAnchored = useAnchoredMenu<HTMLButtonElement, HTMLDivElement>(addMenu, 'right');
  useEffect(() => {
    if (!addMenu) return;
    const onDown = (e: MouseEvent) => {
      if (!addWrapRef.current?.contains(e.target as Node)) setAddMenu(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setAddMenu(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [addMenu]);

  /* ── 消费首页空状态携带的一次性意图（契约「主页 Home · 空状态」：跳 server 页携 `serverPageAction`）──
   *
   * 对齐 上游 `pages/server-page.tsx:177-191`。**读到立刻清**：不清的话用户手动关掉对话框、切走再切回
   * 本页（ScreenRouter 是裸 switch，切屏即重挂）会被同一个意图反复弹窗。清空写在同一个 effect 里，
   * 与「打开对话框」原子发生，中间没有可被别的渲染插入的窗口。 */
  const serverPageAction = useAppStore((s) => s.serverPageAction);
  const setServerPageAction = useAppStore((s) => s.setServerPageAction);
  useEffect(() => {
    if (!serverPageAction) return;
    setServerPageAction(null);
    openDialog(serverPageAction === 'add-server' ? { kind: 'node' } : { kind: 'sub' });
  }, [serverPageAction, setServerPageAction, openDialog]);

  // 分组（单一真值：自建 → 组网 → 各订阅）
  const groups = useMemo(
    () => groupServersBySubscription(servers, subscriptions, true),
    [servers, subscriptions],
  );

  const manualCount = groups.find((g) => g.isManual)?.servers.length ?? 0;
  const meshCount = groups.find((g) => g.isMesh)?.servers.length ?? 0;
  const subCount = servers.length - manualCount - meshCount;
  const statsText = t('nodes.stats', {
    defaultValue: '共 {{total}} · 自建 {{manual}} · 组网 {{mesh}} · 订阅 {{sub}}',
    total: servers.length,
    manual: manualCount,
    mesh: meshCount,
    sub: subCount,
  });

  /**
   * 当前激活 tab（manual/mesh/订阅 id）—— 落地 tab 在**首帧就地派生**，不经 effect 修正。
   *
   * # 为什么初值必须是派生的
   *
   * 原实现是 `useState('manual')` + 一条 `useEffect` 定位到 `initialNodesTab(...)`。`useEffect`
   * 在浏览器**绘制之后**才跑 ⇒ 「自建」那一帧是真的被画出来的：点导航进本页会先看到自建组
   * （只有 1 张自建卡、无订阅信息栏），下一帧才跳到选中节点所在的订阅组 —— 即真机反馈的
   * 「先从自建到实际选中的订阅组一闪而过」。判据（`initialNodesTab`）一直是对的，错的是它被
   * **晚一帧**消费。`useState` 的惰性初值在首次渲染**期间**求值，故首帧画出来的就是正确那组。
   *
   * # 为什么不再需要 effect 补定位
   *
   * 原来那条 effect 的 `if (!want) return` 是为「groups 未水合」留的重试腿，但本屏的 groups 走
   * `groupServersBySubscription(..., true)`，「自建」「组网」两个常驻空组恒在（见该函数
   * `includeEmptyGroups` 注释）⇒ 本屏 groups 恒非空 ⇒ `initialNodesTab` 在此恒有解，
   * 那条腿是死码。`?? 'manual'` 只为吃掉签名里的 `null`，不承载行为。
   *
   * 定位仍**只做一次**（挂载那一次）：之后 tab 归用户，否则用户手动切走后任何一次
   * servers/selected 变动都会把 tab 抢回去。ScreenRouter 离开本页即卸载，故「每次进页面重新定位」
   * 由重挂天然提供 —— 这条语义原先靠 `locatedRef` 守，现在由「初值只求值一次」直接给出。
   */
  const [activeTab, setActiveTab] = useState<string>(
    () => initialNodesTab(groups, selectedServerId) ?? 'manual'
  );
  // 挂载之后的兜底：当前 tab 对应的组消失（订阅被删/清空）→ 回落首组，不留空白页。
  useEffect(() => {
    if (groups.length > 0 && !groups.some((g) => g.id === activeTab)) {
      setActiveTab(groups[0].id);
    }
  }, [groups, activeTab]);

  const activeGroup = groups.find((g) => g.id === activeTab);
  const activeSub: SubscriptionConfig | undefined =
    activeGroup && !activeGroup.isManual && !activeGroup.isMesh
      ? subscriptions.find((s) => s.id === activeGroup.id)
      : undefined;
  // 原型 syncNodeToolbar：isSub = tab id 以 'sub-' 开头；本应用订阅 tab 的 id 就是订阅 id（非 manual/mesh）。
  const isSubTab = !!activeSub;
  // 当前订阅的更新进度（hook 不可条件调用，故非订阅 tab 时以空串取，恒 null）。
  const activeSubProgress = useSubscriptionProgress(activeSub?.id ?? '');

  // 工具栏态。视图档（卡片/列表）是**持久偏好**，不是组件私有 state：ScreenRouter 切屏即卸载重挂，
  // 局部 state 会把用户选的列表视图悄悄改回卡片（见 use-node-view-store 头注）。
  const view = useNodeViewStore((s) => s.view);
  const setView = useNodeViewStore((s) => s.setView);
  const [search, setSearch] = useState('');
  const [protoFilter, setProtoFilter] = useState('');

  // 排序键的「延迟」档 = useNodeSortStore.sortByLatency，**不是**局部 state：原型 :4475 明写「single source of
  // truth: every latency-sort switch (toolbar + Home dropdown) reflects st.latencySort; persisted + tray-synced」，
  // 且 :3012 `if(st.latencySort) st.nodeSort={key:'lat',dir:'asc'}` —— 工具栏排序键由该开关派生。另起局部 state
  // 会让工具栏 / 首页下拉 / 托盘三处各持一份「按延迟排序」，且丢掉持久化（store 已管 localStorage）。
  const sortByLatency = useNodeSortStore((s) => s.sortByLatency);
  const setSortByLatency = useNodeSortStore((s) => s.setSortByLatency);
  // 其余档（默认/名称/协议）无跨端语义（托盘只认「按延迟」与否），留局部态——原型 st.nodeSort 亦不持久化。
  const [nonLatencySortKey, setNonLatencySortKey] = useState<SortKey>('default');
  const sortKey: SortKey = sortByLatency ? 'lat' : nonLatencySortKey;
  const setSortKey = useCallback(
    (key: SortKey) => {
      setSortByLatency(key === 'lat');
      if (key !== 'lat') setNonLatencySortKey(key);
    },
    [setSortByLatency]
  );
  // 多选
  const [batchMode, setBatchMode] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());

  const exitBatch = useCallback(() => {
    setBatchMode(false);
    setSelectedIds(new Set());
  }, []);

  /* 原型 syncNodeToolbar 在切到订阅 tab 时强退批选（`if(isSub && st.batchMode) toggleBatch();`），
     因为原型的批选按钮在订阅 tab 整颗隐藏。本仓改为**订阅 tab 也可批选**（陈先生 2026-07-29 裁定）：
     批选条里「测速所选 / 复制链接」对订阅节点完全成立，被一刀切的守卫连带砍掉是过宽；
     不成立的只有「移动到分组 / 删除」两项，改为在订阅 tab 下不渲染那两颗（见批选条）。
     故此处的自动强退一并撤除 —— 留着会让用户在订阅 tab 刚开批选就被弹出去。 */

  /* ── 设为出口（整卡点击 + 卡上按钮共用这一条腿）──
   *
   * 切换本体走 `useSwitchNode`，与首页出口选单同一份实现（先判后切 / 差集走 pull / toast 互斥）。
   *
   * **默认单击直切，不套二次确认**：`server_switch` 只写 `selectedServerId` + 广播，不重启内核，
   * 且它在暂存层 `BYPASS_TABLE` 里被显式豁免（W-1，理由「首页出口框/状态栏节点名实时回显它」）——
   * 设计上就是同步即时操作。误点的代价是「卡片立刻变 .cur、状态栏节点名变、再点一下切回」，
   * 用高频动作的确认税去防这个是亏的；更要紧的是全仓 `useConfirmTwice` 现在只服务删除/清空/重置，
   * 掺进一个可逆操作会让「点两次 = 有危险」这个信号失效。
   *
   * **唯一例外**：选中「待入池/待生效」差集里的节点会让它由未引用变被引用 ⇒ 恒立即整核重启、
   * 断掉现有连接。那一次确认有信息量，故武装 confirmTwice。
   * 武装判据读 store 快照即可（它只决定「要不要先确认」这一步，滞后一拍最多是少确认一次）；
   * 真正的 toast 分支仍由 `useSwitchNode` 内部按 pull 到的**切换前瞬时**真值决定。
   * 谓词复用 `willRestartOnSelect`（首页预判同一个）—— 在这里另写一份 `added ∪ modified` 就是
   * 把同一条判据分叉成两份，改一处忘一处时两个入口的确认行为会不一致。
   */
  const switchNode = useSwitchNode();
  const pendingChanges = useAppStore((s) => s.pendingChanges);
  const useNode = useCallback(
    (server: ServerConfig, via: NodeUseVia) => {
      const action = nodeUseAction(
        server.id,
        selectedServerId,
        willRestartOnSelect(pendingChanges, server.id),
        via
      );
      if (action === 'noop') return;
      // 显式按钮 + 不重启那档：直切，不收确认税（判据见 nodeUseAction 头注）。
      if (action === 'switch') {
        void switchNode(server.id);
        return;
      }
      // 第一下只武装时给一条 toast —— 整卡点击没有「按钮翻红」那样的就地视觉出口
      // （卡上那颗按钮有 `.confirming`，但用户点的往往是卡面），不提醒就等于点了没反应。
      // `armed` 变了才提醒：第二下（真正执行）不该再弹。
      if (confirmArmed !== `node-use:${server.id}`) {
        toast.info(
          action === 'confirm-restart'
            ? t('nodes.useConfirmRestartToast', {
                node: server.name,
                defaultValue: '再点一次切换到 {{node}} —— 该节点需重启内核，现有连接会断开',
              })
            : t('nodes.useConfirmToast', {
                node: server.name,
                defaultValue: '再点一次切换到 {{node}}',
              })
        );
      }
      confirmTwice(`node-use:${server.id}`, () => void switchNode(server.id));
    },
    [selectedServerId, pendingChanges, confirmArmed, confirmTwice, switchNode, t]
  );

  /**
   * 启动 gate 剔除的非法节点（`EVENT_PROXY_INVALID_NODES` → App.tsx → store）。
   * store 早已存着，但节点卡此前零消费 → 被剔掉的节点在列表里和正常的长得一模一样，
   * 用户选中它、连不上、无从得知原因（上游 `server-card.tsx:58` 是消费的）。
   */
  const invalidNodes = useAppStore((s) => s.invalidNodes);
  const invalidIndex = useMemo(() => invalidNodeIndex(invalidNodes), [invalidNodes]);

  /**
   * 组网同网段「被覆盖（shadowed）」角标（契约·节点角标一节）。
   *
   * 一条 ip_cidr 只能指向一个 outbound，route-builder 按 servers 顺序「首声明者占有」——排在后面
   * 的同段**静默失效**：用户看到两个节点都配了 10.0.0.0/24，却只有一个真的在路由，界面此前不给任何提示。
   *
   * 口径必须与发射端一致（`meshForceRoutedServers` 的 JSDoc 明写本角标与它共用）：只有本轮真会发射
   * force-route 的节点才参与占有/被占判定，否则会对「仅出网且未 engaged」的节点虚报覆盖。
   * `ruleTargetedServerIds` 用 config 原始规则，属该 JSDoc 已登记的 advisory 近似（不在 UI 复刻 backend 的 mode-gate）。
   * 用全量 `servers`（非当前 tab）：订阅里的 endpoint 节点同样参与占有，只看本 tab 会算错归属。
   */
  const ruleTargetedServerIds = useMemo(
    () => collectRuleTargetedServerIds([...(config?.customRules ?? []), ...(config?.appRules ?? [])]),
    [config?.customRules, config?.appRules]
  );
  const shadowedIndex = useMemo(
    () => meshShadowedCidrs(meshForceRoutedServers(servers, selectedServerId, ruleTargetedServerIds)),
    [servers, selectedServerId, ruleTargetedServerIds]
  );
  const serverNameById = useMemo(
    () => new Map(servers.map((s) => [s.id, s.name])),
    [servers]
  );

  // 测速态。结果读**全局 store**、进度走**全局 toast**（两者订阅都在 App.tsx 顶层，切屏不丢）。
  // 本屏只留 `testing` 这一位灰态（按钮禁用），它是本屏控件的属性、天然是组件私有。
  // 勿把 latencies 改回 useState，也勿把进度订阅搬回来——那正是「切屏即丢」的来源。
  const latencies = useLatencyStore((s) => s.latencyMap);
  const applyLatencyResults = useLatencyStore((s) => s.applyLatencyResults);
  const [testing, setTesting] = useState(false);

  const visibleServers = useMemo(() => {
    if (!activeGroup) return [];
    let list = [...activeGroup.servers];
    if (search.trim()) {
      const q = search.trim().toLowerCase();
      list = list.filter(
        (s) => s.name.toLowerCase().includes(q) || s.address.toLowerCase().includes(q),
      );
    }
    if (protoFilter) {
      list = list.filter((s) => s.protocol.toLowerCase() === protoFilter.toLowerCase());
    }
    // 方向按排序键固定为唯一合理值（不可反转，用户定：删掉方向按钮）：名称/协议 A→Z，延迟低→高（快的在前）。
    if (sortKey === 'name') {
      list.sort((a, b) => a.name.localeCompare(b.name));
    } else if (sortKey === 'lat') {
      // 延迟比较委托 domain 单一真值比较器（其 JSDoc 自陈「渲染下拉 + 服务器页共用」）：
      // 无结果恒沉底（不随方向翻转），有效延迟按 order 排序，此处固定 'asc' = 低→高。
      list = sortServersByLatency(list, (id) => latencies[id], 'asc');
    } else if (sortKey === 'proto') {
      list.sort((a, b) => a.protocol.localeCompare(b.protocol));
    }
    return list;
  }, [activeGroup, search, protoFilter, sortKey, latencies]);

  const protoOptions = useMemo(() => {
    if (!activeGroup) return [];
    const set = new Set<string>();
    activeGroup.servers.forEach((s) => set.add(s.protocol));
    return [...set].sort();
  }, [activeGroup]);

  /**
   * 测速可行性的 **path-aware 能力位**（与首页 `HomeScreen:657` 同一个位）：主核 probe 池是否可用
   * = 代理是否在跑。TS-exit 只有主核池路径能测（临时核建不出第二 tsnet 实例），少了这个位
   * 会把「代理没跑时的 TS 节点」当可测发出去，换回一个必然的 `-1`（UI 上读作「真实超时」）。
   */
  const proxyStatus = useAppStore((s) => s.proxyStatus);
  const speedTestCaps = useMemo<SpeedTestCaps>(
    () => ({ mainCorePool: !!proxyStatus?.running }),
    [proxyStatus?.running]
  );

  const speedTestError = useCallback(
    (err: unknown, ctx: string) => {
      console.error(`[NodesScreen] ${ctx} failed:`, err);
      toast.error(speedTestErrorMessage(err, t));
    },
    [t]
  );

  /** 一轮测速的收口：返回值兜底同步进 store（补事件丢失）+ 如实回报缺席节点。四个入口共用。 */
  const absorbRunResult = useCallback(
    (r: SpeedTestInvokeResult) => {
      applyLatencyResults(r.results);
      const msg = notInPoolMessage(r, t);
      if (msg) toast.info(msg);
    },
    [applyLatencyResults, t]
  );

  /**
   * 一轮测速的发起收口：空集不空跑（如实提示），非空才发请求。三个入口共用，只有候选集不同。
   *
   * # 进度**不在本屏画**（此前这里有一行 `nodes.testing` 屏内文本，本轮删掉）
   *
   * 判据是「同一事实两处显示」的收益必须大于成本，而这里三项成本都在、收益是零：
   *  · **收益为零**：进度改成右下角 sticky toast 后，用户站在本屏时两者同框（toast 就在同一个视口里），
   *    屏内那行没有多给任何信息；离开本屏时屏内那行反而是**唯一会消失**的那个。
   *  · **成本一：布局跳动**。那行原本插在页头与 tab 条之间，每轮测速开始时把下面整块内容推低一行、
   *    结束时弹回 —— 用户正想点某张卡时它移位。
   *  · **成本二：口径漂移**。两处各自解读同一份 `{tested, ok, total}`，改文案/改口径时改一处忘一处，
   *    用户会看到两个不一致的数字（本仓已被同型问题咬过多次）。
   *  · **成本三：活的 i18n 洞**。那行用的 `nodes.testing` 只在 en/zh 三语存在，ru/fa 用户读到的是
   *    硬编码中文 defaultValue。删行的同时把该键从三份 locale 里一并清掉（死键不留）。
   *
   * `setTesting(false)` 收在 `finally`：此前它挂在进度事件的 `tested >= total` 上，而后端
   * `runtime/speedtest.rs` 的 `Ok(Some(Err(_))) => {}`（测量任务 JoinError）会让某节点**不落账**
   * ⇒ 即便 outcome 是 `"completed"`，`tested` 也到不了 `total` ⇒ 按钮永久卡在禁用态。
   * invoke 的 settle 时刻**就是**这一轮的结束时刻，是这个灰态唯一无歧义的真值。
   */
  const runSpeedTest = useCallback(
    async (ids: string[], ctx: string) => {
      if (ids.length === 0) {
        toast.info(t('nodes.noTestableNodes', '无可测节点'));
        return;
      }
      setTesting(true);
      try {
        absorbRunResult(await api.server.speedTest(ids));
      } catch (err) {
        speedTestError(err, ctx);
      } finally {
        setTesting(false);
      }
    },
    [absorbRunResult, speedTestError, t]
  );

  /**
   * 页头「全部测速」= **全部已配置节点** 经 `speedTestableIds` 过滤（与首页圆钮 `HomeScreen:657`
   * 逐字同口径）。原实现 `api.server.speedTest()` 不传 id 集、由后端自己决定测谁 —— 同一句
   * 「全部测速」在两屏不同义，且会把结构上测不出真值的节点也测一遍：reverseMesh 的组网节点
   * dial 走 OS default，测出的是**直连的假好值**，却挂在组网节点名下。
   */
  const testAll = useCallback(
    () => runSpeedTest(speedTestableIds(servers, speedTestCaps, stagedOnly), 'speedTest all'),
    [servers, speedTestCaps, stagedOnly, runSpeedTest]
  );

  /**
   * 工具栏「测速」= **当前可见集**（搜索 / 协议筛选之后的 `visibleServers`）∩ 可测集。
   * 原实现测的是 `activeGroup.servers` 整组、无视搜索与协议筛选：用户把 48 个筛到 3 个、
   * 按下按钮，等的却是整组 48 个 —— 与同屏批选腿（测所选）也不是一个口径，**这条射程修正保留**。
   *
   * 文案曾改为「测可见」以把射程写进字面，陈先生 2026-07-29 裁定改回「测速」。
   * 射程不再由按钮文字承载，改由 `testVisibleHint` 浮窗说明（「测速当前筛选后可见的节点」）——
   * 三个入口的字面因此不再互斥，靠位置区分：页头「全部测速」/ 工具栏「测速」（可见）/ 批选条「测速」（所选）。
   */
  const testVisible = useCallback(
    () => runSpeedTest(speedTestableIds(visibleServers, speedTestCaps, stagedOnly), 'speedTest visible'),
    [visibleServers, speedTestCaps, stagedOnly, runSpeedTest]
  );

  /**
   * 批选条的「测速」——只测**所选**（原型 :4134 `batch-test` → `batchSelected(c=>nodeCardTest(c))`）。
   * 此前它直接复用整组全测：勾了 2 个却把整组 48 个全测了，等待时长与结果范围都与操作意图不符。
   * 目标 id 集由 `speedTestIdsForSelection` 单一真值给出（可见 ∩ 已选 ∩ 可测）。
   */
  const testSelected = useCallback(
    () =>
      runSpeedTest(
        speedTestIdsForSelection(visibleServers, selectedIds, speedTestCaps, stagedOnly),
        'speedTest selected'
      ),
    [visibleServers, selectedIds, speedTestCaps, stagedOnly, runSpeedTest]
  );

  // 非活跃节点点测速：后端现在返失败信封（不再静默 ok(empty)）→ 这里必须报，否则点了仍是「没反应」。
  // 卡上的 ⚡ 已按 `speedTestBlockReason` 置灰，不可测节点点不到这里（同一口径，不再另判）。
  const testOne = useCallback(
    async (server: ServerConfig) => {
      try {
        await api.server.speedTest([server.id]);
      } catch (err) {
        speedTestError(err, 'speedTest one');
      }
    },
    [speedTestError]
  );

  /**
   * 不可测原因码 → 已本地化说明（挂在灰 ⚡ 与延迟位上）。措辞本体已下沉
   * `shared/speedtest-feedback.speedTestBlockedMessage` —— 首页「网络检测」在当前出口不可测时
   * 要说同一句话，两处各留一套 switch 就会分叉成「tooltip 一种说法、toast 另一种说法」。
   */
  const blockedHint = useCallback(
    (reason: SpeedTestBlockReason): string => speedTestBlockedMessage(reason, t),
    [t]
  );

  // 后端对 WireGuard/Tailscale/SSH/Custom 明确返错（无标准分享链接形态，见 commands/server.rs），
  // 原先只 console.error → 用户点了毫无反应，与「按钮失灵」无法区分。
  const copyLink = useCallback(
    async (server: ServerConfig) => {
      let url: string;
      try {
        url = await api.server.generateUrl(server);
      } catch (err) {
        console.error('[NodesScreen] generate share url failed:', err);
        toast.error(t('nodes.copyLinkUnsupported', '该协议没有标准分享链接，无法复制'));
        return;
      }
      // 剪贴板失败与「协议无分享链接」是两回事：原先同一个 catch 兜住两者，把「链接生成好了但没写进剪贴板」
      // 谎报成「该协议不支持分享」——用户据此以为协议不支持，实际重试即可。批量版 copyLinksBatch 本就分段，此处对齐。
      try {
        await navigator.clipboard.writeText(url);
        toast.success(t('nodes.copyLinkOk', '已复制分享链接'));
      } catch (err) {
        console.error('[NodesScreen] copy link to clipboard failed:', err);
        toast.error(t('nodes.copyLinksFailed', '复制到剪贴板失败'));
      }
    },
    [t]
  );

  /** 克隆：剥离 id（新建）+ subscriptionId/providerName（克隆体归自建，不随订阅刷新被当差集删除）。 */
  const cloneServer = useCallback(
    async (server: ServerConfig) => {
      // 契约「TS 单例硬限 + WARP 单例硬闸门拦第二个（手输/导入/克隆全经 saveServer）」「TS/WARP 克隆恒撞单例被拦」：
      // 克隆是绕开表单直调 server:add 的一条造节点路径，后端 server_add 无守卫 → 不在此拦就能造出第二实例
      // （TS 多实例互相顶掉 tailnet 地址；WARP 抢内核 utun 致 Connect: resource busy）。
      // 判定走 `meshSingletonConflict`（与三个弹窗同一真值）；文案留克隆专属（「无法克隆出第二个」比
      // 通用的「请先注销现有 WARP」更贴当前动作）。不传 editingId：克隆语义恒为「再加一个」，
      // 源节点自身即占槽 → 必然被拦，与契约一致。
      const slot = meshSingletonConflict(server, servers);
      if (slot) {
        toast.error(
          slot === 'warp'
            ? t('nodes.cloneWarpSingleton', 'WARP 为单例：已存在 WARP 节点，无法克隆出第二个')
            : t('nodes.cloneTsSingleton', 'Tailscale 为单例：同一设备只允许一个 Tailscale 节点，无法克隆')
        );
        return;
      }
      const { id, subscriptionId, providerName, ...rest } = server;
      const cloneName = `${server.name} 副本`;
      try {
        // 配置暂存闸门（与 NodeDialog 同形）。克隆 = 造一个新 `servers` 元素，无任何副作用 ⇒ 默认腿。
        // **删除**那几条腿不同：`server_delete` 会把 WARP 设备推进远端注销队列、清 TS state（W-3），
        // 且连带重选 `selectedServerId`（W-1），故它们留在直写腿上。
        if (editRoute('servers', stagingEnabled) === 'staged') {
          const entityId = crypto.randomUUID();
          stage({
            id: `server:${entityId}`,
            kind: 'server',
            label: `${t('node.addTitle', '添加节点')} ${cloneName}`,
            entityPath: ['servers', entityId],
            nextValue: { ...rest, id: entityId, name: cloneName },
          });
        } else {
          await api.server.add({ ...rest, name: cloneName });
        }
        // 原型 :4514 克隆成功即 notify('已克隆节点','ok')。副本落在**自建**分组（上方剥了 subscriptionId），
        // 当前若停在订阅 tab，新卡不在本 tab 可见 → 无 toast 时点了完全没反应，比原型更需要这条反馈。
        toast.success(t('nodes.cloneSuccess', '已克隆到自建节点'));
      } catch (err) {
        console.error('[NodesScreen] clone failed:', err);
        toast.error(t('nodes.cloneFail', '克隆节点失败'));
      }
    },
    [servers, t, stagingEnabled, stage]
  );

  // Tailscale 登出（契约 L46「tailscale:logout（清 state 保配置）」）：后端命令与 mesh 实现均已就位
  // （server.rs tailscale_logout → runtime/mesh.rs 清 state 目录、保留节点配置/authKey）。
  // 登录态经 setTailscaleLoginState 写回——它是该真值的**唯一入口**（自身文档「STATUS 流 / state 清除事件的
  // 唯一入口」，内部已双写内存态 + localStorage 缓存）。登出正是「state 清除事件」；不写则卡片角标仍显「已登录」。
  const setTailscaleLoginState = useAppStore((s) => s.setTailscaleLoginState);
  const tsLogout = useCallback(
    async (node: ServerConfig) => {
      // `block`（ENTITY_ACTION_TABLE）：登出清的是磁盘上的 TS state 目录，盘上没有这个节点就没有对象。
      const split = splitStagedOnly(
        'server.tailscaleLogout',
        [node.id],
        stagedOnly,
        stagedEntries,
        'servers'
      );
      if (split.blocked.length > 0) {
        toast.info(t('home.stagedOnlyBlocked', '该项还没保存到配置文件，此操作要保存后才能进行。点条上的「保存」后再试'));
        return;
      }
      try {
        await api.server.tailscaleLogout(node.id);
        setTailscaleLoginState(node.id, false);
        toast.success(t('nodes.meshTsLogoutOk', '已登出 Tailscale'));
      } catch (err) {
        console.error('[NodesScreen] tailscale logout failed:', err);
        toast.error(t('nodes.meshTsLogoutFail', '登出 Tailscale 失败'));
      }
    },
    [setTailscaleLoginState, stagedOnly, stagedEntries, t]
  );

  /**
   * 订阅刷新反馈（原型 subRefresh :4777 三分支 notify：304 未变化（中性）/ 已更新 N 个（ok）/ 解析失败（err））。
   * 原型是 Math.random() 三选一的 mock；这里的分支由后端 updateServers 的**真实**返回驱动
   * （契约 §16.3.4 已为此备好 `unchanged` 字段，此前无人消费）。
   * 刷新是纯后台动作：节点数不变时界面一动不动，无 toast 则「点了刷新什么都没发生」——这正是原型要 notify 的原因。
   */
  const refreshSub = useCallback(
    async (sub: SubscriptionConfig) => {
      // 三态 toast 收敛到 domain/subscription-refresh（与 SubDialog 新增后自动拉取共用同一真值）。
      await refreshSubscriptionWithToast(sub.id, t);
    },
    [t]
  );

  /**
   * 删订阅 —— **带二次确认**（原型 subMenu 的删除项明写「删除订阅需二次确认」）。
   * 删订阅不只是删一行：`apply_subscription_delete` 会 `retain` 掉其下**全部节点**，且若当前出口
   * 在其中还会回落直连。此前工具栏的删除按钮直接下手、零确认——与节点删除（早已 confirmTwice）
   * 双标，且后果更大。两个入口（工具栏按钮 + 更多菜单）共用这一条路径。
   */
  const requestSubDelete = useCallback(
    (sub: SubscriptionConfig) => {
      // `disk-only`（ENTITY_ACTION_TABLE）：这个数字承诺「确认后后端会删掉几个」，
      // 而后端级联删的就是磁盘上那些 —— 数 effective 会虚高。
      const count = subDeleteNodeCount(diskServers, sub);
      openDialog({
        kind: 'confirm',
        payload: {
          title: t('nodes.subDeleteTitle', '删除订阅'),
          message: t('nodes.subDeleteConfirm', {
            defaultValue:
              '确定删除订阅 "{{name}}" 吗？其下 {{count}} 个节点将一并移除，此操作无法撤销。',
            name: sub.name,
            count,
          }),
          confirmLabel: t('common.delete', '删除'),
          danger: true,
          onConfirm: () => {
            closeDialog(); // pop confirm
            void api.subscription
              .delete(sub.id)
              .then(() =>
                toast.info(
                  t('nodes.subDeleteOk', {
                    defaultValue: '已删除订阅，移除 {{count}} 个节点',
                    count,
                  }),
                ),
              )
              .catch((err) => {
                // 删失败时订阅 tab 原地不动，与「按钮没反应」不可区分 → 必须透出后端真实原因。
                console.error('[NodesScreen] sub delete:', err);
                toast.error(
                  t('nodes.deleteFail', '删除失败'),
                  err instanceof Error ? err.message : undefined,
                );
              });
          },
        },
      });
    },
    [servers, openDialog, closeDialog, t],
  );

  /**
   * 订阅「更多」菜单动作分派（原型 :4778 `subMenu` 五项）。每一项都落到真链路：
   *  - rename / edit-url → SubDialog（`subscription_update`），autoFocus 到对应输入框；
   *  - copy-url → 剪贴板（纯前端）；
   *  - interval → **无 per-sub 间隔字段**（见 SubInfoBar 头注），如实跳全局设置而非伪造单订阅设置；
   *  - delete → 走上面的二次确认路径。
   */
  const enterSettings = useNavStore((s) => s.enterSettings);
  const onSubMenuAction = useCallback(
    (item: SubMenuItem, sub: SubscriptionConfig) => {
      switch (item) {
        case 'rename':
          openDialog({ kind: 'sub', subId: sub.id, focus: 'name' });
          return;
        case 'edit-url':
          openDialog({ kind: 'sub', subId: sub.id, focus: 'url' });
          return;
        case 'copy-url':
          navigator.clipboard
            .writeText(sub.url)
            .then(() => toast.success(t('nodes.subCopyUrlOk', '已复制订阅 URL')))
            .catch((err) => {
              console.error('[NodesScreen] copy sub url failed:', err);
              toast.error(t('nodes.copyLinksFailed', '复制到剪贴板失败'));
            });
          return;
        case 'interval':
          enterSettings('update');
          return;
        case 'delete':
          requestSubDelete(sub);
      }
    },
    [openDialog, enterSettings, requestSubDelete, t],
  );

  const toggleSelect = useCallback((server: ServerConfig) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(server.id)) next.delete(server.id);
      else next.add(server.id);
      return next;
    });
  }, []);

  const selectAll = useCallback(() => {
    const allSelected =
      selectedIds.size === visibleServers.length && visibleServers.length > 0;
    setSelectedIds(allSelected ? new Set() : new Set(visibleServers.map((s) => s.id)));
  }, [visibleServers, selectedIds.size]);

  /**
   * 删除是不可撤销的破坏性操作 → **原地二次点击**确认（原型 :4140 `node-del`）。
   *
   * 原型这颗是**图标按钮**（卡上的垃圾桶），`confirmTwice(t, '', …)` 的 msg 为空串 ⇒ 无 `<span>`
   * 也无 `data-tip`，确认态**只**靠 `.confirming` 翻红。本仓照搬这条，另把 data-tip/aria-label 换成
   * 「再点一次确认」——纯视觉状态对键盘/读屏用户不可达，那是本仓 DOM 的补齐而非对原型的加戏。
   */
  const requestDelete = useCallback(
    (server: ServerConfig) => {
      confirmTwice(`node-del:${server.id}`, () => {
        // `revert`（ENTITY_ACTION_TABLE）：删一个还没保存的新节点 = 撤销那条条目本身。
        // 策略只写在表里，这里做的是查表 + 按查到的答案分流，不含任何策略判断。
        const split = splitStagedOnly(
          'server.delete',
          [server.id],
          stagedOnly,
          stagedEntries,
          'servers'
        );
        for (const entryId of split.revertEntryIds) revertStaged(entryId);
        if (split.backend.length === 0) {
          // 用户看到的结果与真删除逐字一致（卡片消失）——他删掉的就是他刚加的那个节点。
          toast.info(t('nodes.deleteSuccess', '服务器已删除'));
          return;
        }
        void api.server
          .delete(
            server.id,
            // D4：兜底出口须是**剩余**节点里最快的。原先传 selectedServerId（=被删节点自身）→ 后端 viable
            // 校验恒假 → 恒落直连哨兵，删当前节点即静默裸奔。
            fallbackExitAfterDelete(servers, selectedServerId, new Set([server.id]), latencies)
          )
          // 原型 :4140 node-del 成功即 notify('节点已删除')（中性 kind，非 ok）——删除是用户已预期的结果，
          // 报的是「确实删掉了」而非「恭喜」。
          .then(() => toast.info(t('nodes.deleteSuccess', '服务器已删除')))
          .catch((err) => {
            console.error('[NodesScreen] delete:', err);
            toast.error(t('nodes.deleteFail', '删除失败'));
          });
      });
    },
    [confirmTwice, servers, selectedServerId, latencies, stagedOnly, stagedEntries, revertStaged, t]
  );

  /**
   * WARP 设备注销 / 重新注册的共用腿：**删掉 WARP 节点**。
   *
   * 为什么"注销设备" = 删节点：`server_delete` 的
   * `run_server_removal_side_effects` 会把节点上的 `wireguardSettings.warpDevice`（deviceId+token）
   * 推进待注销队列，由 `MeshRuntime::spawn_warp_drain` 向 Cloudflare 发 `DELETE /reg/{id}` 真注销
   * （凭据/退避/超龄丢弃逻辑全在 `crates/mesh/src/warp.rs`，已有单测）。这与 上游 完全同构——
   * 上游 也没有独立的"注销"命令，注销就是删节点时的副作用（`WarpDeregisterQueue`）。
   * 故这里不新造后端命令，而是把原型的菜单项映射到既有真链路上。
   *
   * `afterDelete` 用于「重新注册」：删完旧的再开注册向导（WARP 是单例槽，不先删就会被
   * `warpSlotTaken` 硬闸门拦下）。
   */
  const removeWarpNode = useCallback(
    (node: ServerConfig, opts: { title: string; message: string; okToast: string; afterDelete?: () => void }) => {
      openDialog({
        kind: 'confirm',
        payload: {
          title: opts.title,
          message: opts.message,
          confirmLabel: t('common.confirm', '确定'),
          danger: true,
          onConfirm: () => {
            closeDialog(); // pop confirm
            // 经同一条表查询（`server.delete`）而不是直发后端：WARP 节点今天恒直落盘
            // （写侧守卫把本族四条写腿全定为 `direct`/W-3）⇒ `stagedOnly` 里不会有它 ⇒ 恒走 backend 腿、
            // 与今天逐字节相同。走一遍表是为了不留「同一个动作两套判据」的分叉。
            const split = splitStagedOnly(
              'server.delete',
              [node.id],
              stagedOnly,
              stagedEntries,
              'servers'
            );
            for (const entryId of split.revertEntryIds) revertStaged(entryId);
            if (split.backend.length === 0) {
              toast.info(opts.okToast);
              opts.afterDelete?.();
              return;
            }
            void api.server
              .delete(
                node.id,
                fallbackExitAfterDelete(servers, selectedServerId, new Set([node.id]), latencies),
              )
              .then(() => {
                toast.info(opts.okToast);
                opts.afterDelete?.();
              })
              .catch((err) => {
                console.error('[NodesScreen] warp remove failed:', err);
                toast.error(t('nodes.deleteFail', '删除失败'));
              });
          },
        },
      });
    },
    [openDialog, closeDialog, servers, selectedServerId, latencies, stagedOnly, stagedEntries, revertStaged, t],
  );

  /**
   * 全局「添加」菜单中的组网接入腿。所有 Tab 共用同一个入口；打开前先切到组网，提交后新增卡片
   * 直接出现在用户当前所见的分组里。接入协议选择继续由 MeshJoinDialog 承担，避免把五种协议铺满菜单。
   */
  const openMeshJoin = useCallback(() => {
    setActiveTab('mesh');
    openDialog({
      kind: 'mesh-join',
      onTsLogout: (node) => void tsLogout(node),
      onWarpReregister: (node) =>
        removeWarpNode(node, {
          title: t('nodes.meshWarpReRegisterTitle', '重新注册 WARP'),
          message: t(
            'nodes.meshWarpReRegisterMsg',
            '将先注销当前 WARP 设备（并移除该节点），再开始注册一台新的匿名设备。当前设备的 WARP+ 许可不会转移到新设备。',
          ),
          okToast: t('nodes.meshWarpReRegisterOk', '已注销旧设备，请继续注册'),
          afterDelete: () => openDialog({ kind: 'warp', edit: false }),
        }),
      onWarpDeregister: (node) =>
        removeWarpNode(node, {
          title: t('nodes.meshWarpDeregisterTitle', '注销 WARP 设备'),
          message: t(
            'nodes.meshWarpDeregisterMsg',
            '将向 Cloudflare 注销此匿名设备并移除该节点，此操作无法撤销。若已绑定 WARP+ 许可，注销后需要重新绑定到新设备。',
          ),
          okToast: t('nodes.meshWarpDeregisterOk', '已注销 WARP 设备'),
        }),
    });
  }, [openDialog, removeWarpNode, tsLogout, t]);

  /** 批量删除 —— 原地二次点击（原型 :4137 `batch-del`，确认文案「删除选中节点？」直接换在按钮上）。 */
  const deleteBatch = useCallback(() => {
    if (selectedIds.size === 0) return;
    const ids = new Set(selectedIds);
    confirmTwice(BATCH_DEL_KEY, () => {
      void (async () => {
        // 同 requestDelete：一批里 staged-only 与盘上实体混在一起，两半各走各的腿。
        const split = splitStagedOnly(
          'server.deleteBatch',
          [...ids],
          stagedOnly,
          stagedEntries,
          'servers'
        );
        for (const entryId of split.revertEntryIds) revertStaged(entryId);
        if (split.backend.length === 0) {
          exitBatch();
          toast.info(
            t('nodes.batchDeleteOk', { count: ids.size, defaultValue: '已删除 {{count}} 个节点' })
          );
          return;
        }
        try {
          // 同 D4：兜底出口从**删除集之外**的剩余节点里取最快（后端本批新增 fallback_selected_id 形参，
          // 此前该 key 被 Tauri 静默丢弃、批删掉当前出口恒落直连）。
          await api.server.deleteBatch(
            [...split.backend],
            fallbackExitAfterDelete(servers, selectedServerId, ids, latencies)
          );
          exitBatch();
          // 原型 :4137 batch-del 成功即 notify('已删除')（中性）。这里带上条数：批删同时退出批选模式、
          // 选中态一并清空，用户失去「刚才选了几个」的参照，报数才对得上账。
          toast.info(
            t('nodes.batchDeleteOk', { count: ids.size, defaultValue: '已删除 {{count}} 个节点' })
          );
        } catch (err) {
          console.error('[NodesScreen] batch delete failed:', err);
          toast.error(t('nodes.deleteFail', '删除失败'));
        }
      })();
    });
  }, [selectedIds, confirmTwice, servers, selectedServerId, latencies, exitBatch, stagedOnly, stagedEntries, revertStaged, t]);

  // allSettled 而非 all：批选里混进一个无分享链接形态的协议（WG/TS/SSH/Custom），all 会整体 reject
  // → 本可成功的链接一条都进不了剪贴板。改为能复制的照常复制，跳过的如实报数。
  const copyLinksBatch = useCallback(async () => {
    if (selectedIds.size === 0) return;
    const targets = visibleServers.filter((s) => selectedIds.has(s.id));
    const settled = await Promise.allSettled(targets.map((s) => api.server.generateUrl(s)));
    const urls = settled
      .filter((r): r is PromiseFulfilledResult<string> => r.status === 'fulfilled')
      .map((r) => r.value);
    const skipped = settled.length - urls.length;
    if (urls.length === 0) {
      toast.error(t('nodes.copyLinkUnsupported', '该协议没有标准分享链接，无法复制'));
      return;
    }
    try {
      await navigator.clipboard.writeText(urls.join('\n'));
      toast.success(
        skipped > 0
          ? t('nodes.copyLinksPartial', { count: urls.length, skipped, defaultValue: '已复制 {{count}} 条，{{skipped}} 条不支持分享链接已跳过' })
          : t('nodes.copyLinksOk', { count: urls.length, defaultValue: '已复制 {{count}} 条分享链接' })
      );
    } catch (err) {
      console.error('[NodesScreen] batch copy links failed:', err);
      toast.error(t('nodes.copyLinksFailed', '复制到剪贴板失败'));
    }
  }, [visibleServers, selectedIds, t]);

  return (
    <section
      id="s-nodes"
      className={cn('screen', view === 'list' && 'nodes-list-view', batchMode && 'nodes-batch-mode')}
    >
      <NdFlagDefs />

      {/* phead */}
      <div className="phead">
        <div className="ph-title">
          <h1>{t('sidebar.server', '节点')}</h1>
          <span className="nd-count">{statsText}</span>
        </div>
        <div className="acts">
          <button
            type="button"
            className="btn ghost"
            onClick={testAll}
            disabled={testing}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M13 2L4 14h6l-1 8 9-12h-6z" />
            </svg>
            <span>{t('nodes.testAll', '全部测速')}</span>
          </button>
          <div ref={addWrapRef} style={{ position: 'relative' }}>
            <button
              ref={addAnchored.anchorRef}
              type="button"
              className="btn flow"
              aria-haspopup="menu"
              aria-expanded={addMenu}
              onClick={() => setAddMenu((v) => !v)}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M12 5v14M5 12h14" />
              </svg>
              <span>{t('nodes.add', '添加')}</span>
              <svg viewBox="0 0 24 24" className="nd-chev" fill="none" stroke="currentColor" strokeWidth={1.9}>
                <path d="M6 9l6 6 6-6" />
              </svg>
            </button>
            {addMenu && (
              <div
                ref={addAnchored.menuRef}
                className="mini-menu"
                role="menu"
                style={addAnchored.style}
              >
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  onClick={() => {
                    setAddMenu(false);
                    setActiveTab('manual');
                    openDialog({ kind: 'node' });
                  }}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M12 20h9M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z" />
                  </svg>
                  <span>{t('nodes.manualAdd', '添加代理节点')}</span>
                </button>
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  onClick={() => {
                    setAddMenu(false);
                    openMeshJoin();
                  }}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M9 15l6-6M8 8a3 3 0 10-3 3M16 16a3 3 0 103 3" />
                  </svg>
                  <span>{t('nodes.meshAddAccess', '添加组网接入')}</span>
                </button>
                <div className="mm-sep" role="separator" />
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  onClick={() => {
                    setAddMenu(false);
                    openDialog({ kind: 'import' });
                  }}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M12 15V4M8 8l4-4 4 4M4 15v3a2 2 0 002 2h12a2 2 0 002-2v-3" />
                  </svg>
                  <span>{t('nodes.manualImport', '导入节点配置')}</span>
                </button>
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  onClick={() => {
                    setAddMenu(false);
                    openDialog({ kind: 'sub', onAdded: setActiveTab });
                  }}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M4 11a9 9 0 019 9M4 4a16 16 0 0116 16" />
                  </svg>
                  <span>{t('nodes.addSubscription', '添加订阅')}</span>
                </button>
              </div>
            )}
          </div>
        </div>
      </div>

      {/* 订阅 tabs */}
      <div className="nd-tabs-scroll" id="node-tabs-scroll">
        <div className="sub-tabs" data-tabgroup="">
          {groups.map((g) => {
            const label = g.isManual
              ? t('nodes.tab.manual', '自建节点')
              : g.isMesh
                ? t('nodes.tab.mesh', '组网')
                : g.name;
            return (
              <button
                key={g.id}
                type="button"
                className={cn(activeTab === g.id && 'on')}
                data-act="sub-tab"
                data-v={g.id}
                onClick={() => setActiveTab(g.id)}
              >
                <span>{label}</span>
                {g.servers.length > 0 && <span className="cnt">{g.servers.length}</span>}
              </button>
            );
          })}
        </div>
      </div>

      {/* 订阅信息栏 */}
      {activeSub && (
        <div className="nd-subinfo">
          <SubInfoBar
            subscription={activeSub}
            nodeCount={activeGroup?.servers.length ?? 0}
            config={config ?? undefined}
            deleteNodeCount={subDeleteNodeCount(diskServers, activeSub)}
            progress={activeSubProgress}
            onEdit={(sub) => openDialog({ kind: 'sub', subId: sub.id })}
            onRefresh={(sub) => void refreshSub(sub)}
            onDelete={requestSubDelete}
            onMenuAction={onSubMenuAction}
          />
        </div>
      )}

      {/* 工具栏 */}
      <div className="node-toolbar" id="node-shared-tools">
        <div className="seg2 nh-view" role="group" aria-label="View">
          <button
            type="button"
            className={cn(view === 'cards' && 'on')}
            onClick={() => setView('cards')}
            aria-label={t('nodes.view.cards', '卡片视图')}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <rect x="3" y="3" width="8" height="8" rx="1.5" />
              <rect x="13" y="3" width="8" height="8" rx="1.5" />
              <rect x="3" y="13" width="8" height="8" rx="1.5" />
              <rect x="13" y="13" width="8" height="8" rx="1.5" />
            </svg>
          </button>
          <button
            type="button"
            className={cn(view === 'list' && 'on')}
            onClick={() => setView('list')}
            aria-label={t('nodes.view.list', '列表视图')}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M8 6h13M8 12h13M8 18h13M3.5 6h.01M3.5 12h.01M3.5 18h.01" />
            </svg>
          </button>
        </div>

        <label className="input search-box nh-search">
          <svg viewBox="0 0 24 24" width={15} fill="none" stroke="currentColor" strokeWidth={1.8}>
            <circle cx="11" cy="11" r="7" />
            <path d="M20 20l-3-3" />
          </svg>
          <input
            id="node-search"
            type="search"
            placeholder={t('nodes.search.placeholder', '搜索节点名称、地址…')}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </label>

        <Csel
          className="nt-proto"
          id="node-proto-filter"
          ariaLabel={t('nodes.filter.protocol', '协议')}
          value={protoFilter}
          onChange={setProtoFilter}
          options={[
            { value: '', label: t('nodes.filter.allProto', '全部协议') },
            ...protoOptions.map((p) => ({ value: p, label: p })),
          ]}
        />

        <Csel
          className="nh-sort"
          id="node-sort"
          ariaLabel={t('nodes.sortBy', '排序方式')}
          value={sortKey}
          onChange={(v) => setSortKey(v as SortKey)}
          options={[
            { value: 'default', label: t('nodes.sort.default', '默认顺序') },
            { value: 'name', label: t('nodes.sort.name', '名称') },
            { value: 'lat', label: t('nodes.sort.latency', '延迟') },
            { value: 'proto', label: t('nodes.sort.protocol', '协议') },
          ]}
        />

        {/* 测的是搜索/协议筛选之后**你眼前这些**（∩ 可测集），不是整组。
            射程由 `data-tip` 说明承载，不写进按钮字面（陈先生 2026-07-29 裁定）。 */}
        <button
          type="button"
          className="btn ghost sm"
          onClick={testVisible}
          disabled={testing}
          data-tip={t('nodes.testVisibleHint', '测速当前筛选后可见的节点')}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M13 2L4 14h6l-1 8 9-12h-6z" />
          </svg>
          <span>{t('nodes.testVisible', '测速')}</span>
        </button>

        {/* 多选。原型 `.nt-hide-sub` 在订阅 tab 整颗隐藏，**本仓不再隐藏**（理由见上方 syncNodeToolbar
            那段注释）：批选条按 tab 裁动作，而不是按 tab 裁掉整个批选能力。 */}
        <button
          type="button"
          id="batch-toggle"
          className={cn('btn ghost sm nt-hide-sub', batchMode && 'on')}
          aria-pressed={batchMode}
          onClick={() => (batchMode ? exitBatch() : setBatchMode(true))}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M9 11l3 3L22 4" />
            <path d="M21 12v7a2 2 0 01-2 2H5a2 2 0 01-2-2V5a2 2 0 012-2h11" />
          </svg>
          <span>{t('nodes.batch', '多选')}</span>
        </button>
      </div>

      {/* 批选操作条 */}
      {batchMode && (
        <div className="batch-bar" id="nodes-batch">
          <button
            type="button"
            id="batch-all"
            className="nd-check on"
            role="checkbox"
            aria-checked={selectedIds.size === visibleServers.length && visibleServers.length > 0}
            onClick={selectAll}
            style={{ position: 'static' }}
            aria-label={t('nodes.selectAll', '全选')}
          >
            {selectedIds.size === visibleServers.length && visibleServers.length > 0 && (
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
                <path d="M5 12l5 5 9-11" />
              </svg>
            )}
          </button>
          <b>
            {t('nodes.selectedPrefix', '已选 ')}
            <span className="mono">{selectedIds.size}</span>
            {t('nodes.selectedSuffix', ' 个')}
          </b>
          <div className="sp" />
          <button
            type="button"
            id="batch-test"
            className="btn ghost sm"
            onClick={testSelected}
            disabled={testing || selectedIds.size === 0}
          >
            {t('nodes.testGroup', '测速')}
          </button>
          {/*
            「移动到分组」恒禁用 —— **诚实置灰，不是待办**。根因不是"后端命令还没写"，而是
            Polaris 数据模型里没有用户可分配的分组：自建/组网/订阅三类归属全是派生的
            （无 subscriptionId / endpoint 协议 / subscriptionId 指向订阅），唯一可写的
            `subscriptionId` 一旦写进某订阅，下次订阅刷新的 reconcile 会把该节点当"已下架"删掉
            （subscription.rs:755 按 subscriptionId 分区整体替换）= 数据丢失。上游 全仓亦无
            move-to-group，原型那项是 notify('已移动') 的纯 mock。
            判定收在 nodes-logic.canMoveToGroup（引入真分组字段即自动解禁）。
            订阅 tab 下整颗不渲染：那里连"理论上想移动"都不成立，多摆一颗恒灰的按钮只是噪声。
          */}
          {!isSubTab && (
            <button
              type="button"
              id="batch-move"
              className="btn ghost sm"
              disabled={!canMoveToGroup()}
              data-tip={t(
                'nodes.batchMoveUnavailable',
                'Polaris 的分组由订阅归属与协议派生，没有可自由分配的分组；把节点写进订阅分组会在下次订阅刷新时被当作已下架删除，故不提供此操作',
              )}
            >
              {t('nodes.batchMove', '移动到分组')}
            </button>
          )}
          <button
            type="button"
            className="btn ghost sm"
            onClick={copyLinksBatch}
            disabled={selectedIds.size === 0}
          >
            {t('nodes.batchCopyLinks', '复制链接')}
          </button>
          {/* 订阅 tab 下不渲染：删掉的订阅节点会在下次订阅刷新的 reconcile 里原样拉回来
              ⇒ 操作无净效果、只剩误删风险（陈先生 2026-07-29 裁定，与单卡删除入口同一处置）。 */}
          {!isSubTab && (
            <button
              type="button"
              className={cn('btn ghost sm', confirmArmed === BATCH_DEL_KEY && 'confirming')}
              style={{ color: 'hsl(var(--err))' }}
              onClick={deleteBatch}
              disabled={selectedIds.size === 0}
            >
              {confirmArmed === BATCH_DEL_KEY
                ? t('nodes.batchDeleteConfirmAgain', '删除选中节点？')
                : t('common.delete', '删除')}
            </button>
          )}
          <button type="button" className="btn ghost sm" onClick={exitBatch}>
            {t('nodes.batchExit', '退出')}
          </button>
        </div>
      )}
      {/* 节点网格 */}
      <div className="node-grid">
        {visibleServers.length === 0 ? (
          <div className="stub" style={{ gridColumn: '1 / -1' }}>
            {/* 空态文案按**组的语义**分流：订阅组的节点由订阅拉取而来，用户在这里点右上「添加」
                只会造出一个自建节点、订阅照旧是空的 —— 该引导他刷新订阅（0 节点订阅也保留 tab，
                见 server-grouping 的 includeEmptyGroups 注释，正是为了留住刷新/删除入口）。 */}
            <p>
              {search || protoFilter
                ? t('nodes.emptyFiltered', '无匹配节点')
                : activeSub
                  ? t('nodes.emptySub', '该订阅当前没有节点，点上方「刷新」重新拉取')
                  : activeGroup?.isMesh
                    ? t('nodes.meshEmpty', '暂无组网接入，请从右上角「添加 → 组网接入」开始配置')
                  : t('nodes.empty', '暂无节点，点右上「添加」')}
            </p>
          </div>
        ) : (
          visibleServers.map((server) => {
            const isMesh = activeGroup?.isMesh || isMeshNode(server);
            // 「仅局域网」角标走 domain 谓词 `meshAllowsInternet`，不再自造
            // `wireguardSettings.allowInternet === false` 弱判定 —— 后者漏掉 Tailscale 这一族
            // （TS 的「有没有外网出口」由 exitNode 派生），无出口的 TS 节点此前既不显角标、⚡ 也照样亮。
            const lanOnly = isMesh && !meshAllowsInternet(server);
            // ⚡ 与延迟位的可测性：与页头/工具栏/批选**同一条**过滤线（`isSpeedTestable`）。
            const blockReason = speedTestBlockReason(server, speedTestCaps, stagedOnly.has(server.id));
            // byId → 显示名（节点可能已被删/不在本 tab；取不到就退回 id，别让 tooltip 变成空引号）。
            const shadowed = shadowedIndex
              .get(server.id)
              ?.map((s) => ({ cidr: s.cidr, by: serverNameById.get(s.byId) ?? s.byId }));
            return (
              <NodeCard
                key={server.id}
                server={server}
                latencyMs={latencies[server.id]}
                isCurrent={server.id === selectedServerId}
                isExit={isMesh}
                lanOnly={lanOnly}
                speedTestable={blockReason === null}
                speedTestBlockedHint={blockReason ? blockedHint(blockReason) : undefined}
                shadowedCidrs={shadowed}
                selected={selectedIds.has(server.id)}
                batchMode={batchMode}
                invalidReason={invalidIndex[server.id]}
                stagedOnly={stagedOnly.has(server.id)}
                onSpeedTest={testOne}
                onCopy={copyLink}
                onClone={cloneServer}
                onEdit={(s) => openDialog(editDialogFor(s))}
                onUse={useNode}
                useConfirming={confirmArmed === `node-use:${server.id}`}
                useWillRestart={willRestartOnSelect(pendingChanges, server.id)}
                onDelete={requestDelete}
                /* 订阅节点不给删除入口：删了下次订阅刷新 reconcile 会原样拉回来。 */
                deletable={!server.subscriptionId}
                deleteConfirming={confirmArmed === `node-del:${server.id}`}
                onToggleSelect={toggleSelect}
              />
            );
          })
        )}
      </div>
    </section>
  );
}

export default NodesScreen;
