/**
 * 节点屏纯逻辑 —— 抽出以便 node 环境 vitest 直测（本仓 vitest `environment:'node'`，无 jsdom）。
 *
 * 同 `settings/settings-logic.ts` 的既有约定：这些判定全是「UI 显示态 ↔ 后端实际行为」的对齐点，
 * 写错的后果不是样式歪，而是**界面说的和后端做的不一致**（最恶劣：pill 显示"自动刷新中"而调度器
 * 根本没在跑，或反过来）。组件**直接消费**这里的导出，不并行复刻，否则测试是假绿。
 */

import type { ServerConfig, SubscriptionConfig, InvalidNodeInfo } from '@/contracts/types';
import {
  isMeshNode,
  isSpeedTestable,
  meshAllowsInternet,
  meshUsesSystemInterface,
  speedTestableIds,
  type SpeedTestCaps,
} from '@/domain/endpoint-routes';

/* ────────────────────────────────────────────────────────────────────────────
 * #1 订阅自动更新徽标三态（原 SubInfoBar 的"尚未接入调度器"提示已过期）
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 订阅自动刷新的**真实**生效态。
 *
 * 背景（陈旧提示的根因）：`SubInfoBar` 原先给 `autoUpdate` 的订阅挂一句
 * 「尚未接入调度器：不会真正按此周期自动刷新」——**该判断早已不成立**。
 * `runtime/subscription_scheduler.rs::select_due` 逐条消费 per-sub `autoUpdate`（:148），
 * 是真在跑的。但它同时受两个上游门控，任一不满足这条订阅就确实不会被周期刷新：
 *
 *  1. 总开关 `autoUpdateSubscriptionOnStart`（:122 `!= Some(true)` → 整轮空）；
 *  2. `subscriptionUpdateIntervalHours === 0`（UI「仅手动」档，:135 → 周期巡检腿一律空；
 *     但**启动补更腿不受影响**，故这一档不是"完全不自动"，而是"只在启动时更新"）。
 *
 * 故三态而非布尔——绿点只在"真会按周期刷新"时给，其余给中性点 + 诚实说明。
 * 与 `settings-logic.ts::ruleResourceAutoStatus` 同一套「开关开 ≠ 真会刷新」口径。
 */
export type SubAutoUpdateStatus =
  /** per-sub 开关关 → 纯手动。 */
  | 'manual'
  /** per-sub 开着，但全局总开关关 → 不会自动刷新。 */
  | 'master-off'
  /** per-sub + 总开关都开，但间隔为「仅手动」→ 只在启动时补更，不按周期。 */
  | 'startup-only'
  /** 真会按周期自动刷新。 */
  | 'active';

/** 调度器缺省间隔（`subscription_scheduler.rs::DEFAULT_INTERVAL_HOURS`）。 */
export const SUB_DEFAULT_INTERVAL_HOURS = 12;

export interface SubAutoUpdateConfigLike {
  autoUpdateSubscriptionOnStart?: boolean;
  subscriptionUpdateIntervalHours?: number;
}

export function subAutoUpdateStatus(
  sub: { autoUpdate?: boolean },
  config: SubAutoUpdateConfigLike | null | undefined,
): SubAutoUpdateStatus {
  if (sub.autoUpdate !== true) return 'manual';
  // 后端是 `!= Some(true)`（严格真才跑）——UI 必须同口径，不能用 `!== false`（缺省为开）。
  if (config?.autoUpdateSubscriptionOnStart !== true) return 'master-off';
  if (config?.subscriptionUpdateIntervalHours === 0) return 'startup-only';
  return 'active';
}

/**
 * 徽标上显示的间隔小时数。后端 `.filter(|h| *h > 0).unwrap_or(12)`：非正数 / 缺省 → 12h。
 * 原先徽标写死 "自动 · 12h"，用户把间隔改成 24h 后徽标仍显 12h（说了假话）。
 */
export function subEffectiveIntervalHours(config: SubAutoUpdateConfigLike | null | undefined): number {
  const h = config?.subscriptionUpdateIntervalHours;
  return typeof h === 'number' && Number.isFinite(h) && h > 0 ? h : SUB_DEFAULT_INTERVAL_HOURS;
}

/* ────────────────────────────────────────────────────────────────────────────
 * #3 测速口径：三个入口同一条过滤线
 *
 * 节点屏有三个测速入口（页头「全部测速」/ 工具栏「测速」（可见集）/ 批选条「测速」）+ 每张卡上的 ⚡。
 * 四者的**候选集**不同（全量 / 可见 / 可见∩已选 / 单节点），但**过滤口径必须只有一条**：
 * `isSpeedTestable`（domain 单一真值，首页两条腿已消费）。
 *
 * 曾经的形态：节点屏零消费该谓词，自造 `lanOnly = allowInternet===false` 弱判定 —— reverseMesh
 * 的 WG（dial 走 OS default，测出**直连的假好值**挂在组网节点名下）与无 exitNode 的 Tailscale
 * （公网黑洞必假超时）照样显 ⚡ 可点；页头「全部测速」还不传 id 集（后端全量），
 * 同一句「全部测速」在首页与节点页**不同义**。
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 「本屏这一刻要不要订整张延迟表」的选择器 —— 排序键不是 `lat` 时恒返回**同一个**空表哨兵。
 *
 * # 为什么必须是模块级常量，而不是 `{}` 字面量或 `useMemo`
 *
 * zustand 默认用 `Object.is` 比较选择器的返回值来决定「要不要让这个组件重渲」。选择器里写
 * `: {}` 每次求值都是新对象 ⇒ 每一次 store 提交（测速逐节点回包，一轮几十上百次）都判不等 ⇒
 * 父层照样每次提交重渲、`visibleServers` 照样整表重算、所有卡片跟着重渲 —— 与不做条件订阅
 * **逐字等价**，但多骗了一次 code review。`useMemo` 也不行：hook 只在组件里能调，而这里要的是
 * 「跨组件实例、跨渲染恒等」的一个引用，只有模块级常量给得了。
 *
 * # 为什么不能反过来「恒不订、动作腿现取」
 *
 * `sortKey === 'lat'` 时排序结果**必须**随每次回包同步重排（不变量①），那时父层就该重渲。
 * 条件订阅要收掉的是另外三档排序（默认/名称/协议）下那份「订了也没人读」的重渲，不是排序本身。
 */
export const EMPTY_LATENCY_MAP: Readonly<Record<string, number | null>> = Object.freeze({});

/** [`EMPTY_LATENCY_MAP`] 的消费面：`useLatencyStore(latencySortSelector(sortKey === 'lat'))`。 */
export function latencySortSelector(
  needsLatency: boolean
): (s: { latencyMap: Record<string, number | null> }) => Readonly<Record<string, number | null>> {
  return (s) => (needsLatency ? s.latencyMap : EMPTY_LATENCY_MAP);
}

/**
 * 批选测速的目标 id 集 = **当前可见列表里被选中、且结构上可测**的那些（按可见顺序）。
 *
 * 两层过滤，缺一不可：
 *  - **可见 ∩ 已选**（而非直接 `[...selectedIds]`）：选完后改搜索/协议筛选会让部分选中项移出视野，
 *    此时用户看到的是"已选 N 个"却在测他看不见的节点。与批量删除/复制链接
 *    （`visibleServers.filter(s => selectedIds.has(s.id))`）同口径。
 *    原实现更粗：批选条的「测速」直接复用整组全测，选了 2 个却测 48 个（原型 :4134 `batch-test`
 *    明写 `batchSelected(c=>nodeCardTest(c))`，只测选中卡片）。
 *  - **`speedTestableIds`**：勾中的节点里可能混着结构上测不出真值的（reverseMesh / mesh-only /
 *    custom endpoint / 主核未跑的 TS-exit），它们必返 `-1`，而 `-1` 在 UI 上读作「真实超时」
 *    而非「未测」⇒ 伪造数值。与首页/页头/工具栏同一条过滤线。
 */
export function speedTestIdsForSelection(
  visible: readonly ServerConfig[],
  selectedIds: ReadonlySet<string>,
  caps?: SpeedTestCaps,
  excludeIds?: ReadonlySet<string>,
): string[] {
  return speedTestableIds(
    visible.filter((s) => selectedIds.has(s.id)),
    caps,
    excludeIds,
  );
}

/**
 * 节点**为什么**不可测（`isSpeedTestable` 为假时的原因码）。可测则返 `null` ——
 * `null ⟺ isSpeedTestable` 是本函数的不变量（单测钉死），调用方据此单点判定，不再另起判据。
 *
 * 存在的理由：置灰不给理由等于没说话。⚡ 灰着而用户不知道是「Tailscale 没设出口节点」还是
 * 「代理没跑」还是「这个节点绑了 System 内核接口」，就只会去点第二次、第三次。
 * 分支顺序与 `isSpeedTestable` 的判定顺序严格镜像，否则会给出与置灰理由不符的解释。
 */
export type SpeedTestBlockReason =
  /** reverseMesh：绑了 System 内核接口，dial 走 OS default → 会测出直连的假好值。 */
  | 'system-interface'
  /** Tailscale 未设出口节点（exit node）→ 无公网出口，探测进黑洞必假超时。 */
  | 'ts-no-exit'
  /** WireGuard / WARP 关了「允许访问外网」→ 只承载具体网段，公网探测被 cryptokey routing 丢弃。 */
  | 'lan-only'
  /** TS-exit 仅主核池路径可测：代理没在跑（临时核建不出第二 tsnet 实例）。 */
  | 'ts-core-not-ready'
  /** custom endpoint：raw JSON 无 gate 真值，测不出可信数值。 */
  | 'custom-endpoint'
  /** 该节点只在暂存里、磁盘上还没有 ⇒ 运行核里没有它，测不出真值（`ENTITY_ACTION_TABLE` 的 `block` 腿）。 */
  | 'staged-only'
  /** 兜底：`isSpeedTestable` 将来新增排除分支而本函数未跟上时，仍给通用「不支持测速」而非谎称可测。 */
  | 'other';

/**
 * `stagedOnly`（可选）**先于**结构可测性判：它问的不是「这个节点能不能测」，而是「盘上到底有没有它」。
 * 一个 staged-only 的 VLESS 节点结构上完全可测，只是后端按 id 查不到 —— 不先判就会给出
 * 「不支持测速」这种与事实无关的解释。不变量随之扩成 `null ⟺ isSpeedTestable ∧ ¬stagedOnly`。
 */
export function speedTestBlockReason(
  server: ServerConfig,
  caps?: SpeedTestCaps,
  stagedOnly?: boolean,
): SpeedTestBlockReason | null {
  if (stagedOnly) return 'staged-only';
  if (isSpeedTestable(server, caps)) return null;
  const p = server.protocol?.toLowerCase();
  if (meshUsesSystemInterface(server)) return 'system-interface';
  if (isMeshNode(server) && !meshAllowsInternet(server)) {
    return p === 'tailscale' ? 'ts-no-exit' : 'lan-only';
  }
  if (p === 'tailscale') return 'ts-core-not-ready';
  if (p === 'custom' && server.customSettings?.isEndpoint) return 'custom-endpoint';
  return 'other';
}

/* ────────────────────────────────────────────────────────────────────────────
 * #4 启动 gate 剔除的非法节点（invalidNodes）
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * `InvalidNodeInfo[]` → `id → reason` 索引，供节点卡 O(1) 查（上游 `server-card.tsx:58`
 * `invalidNodes[server.id]` 同形）。
 *
 * store 里 `invalidNodes` 早已由 `EVENT_PROXY_INVALID_NODES` 填好，但节点卡零消费 →
 * 被内核 gate 剔掉的节点在列表里和正常节点长得一模一样，用户选它、连不上、无从得知原因。
 */
export function invalidNodeIndex(
  nodes: readonly InvalidNodeInfo[] | null | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const n of nodes ?? []) {
    if (!n?.id) continue;
    // reason 可能为空串（后端只给了 tag）——仍要建索引（置灰照做），tooltip 由消费方兜底文案。
    out[n.id] = n.reason ?? '';
  }
  return out;
}

/* ────────────────────────────────────────────────────────────────────────────
 * #2 批量「移动到分组」为什么恒禁用
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 批量移动分组的**可选目标分组**。恒返回空数组——不是"后端命令还没写"，而是
 * **Polaris 的数据模型里没有用户可分配的分组**：
 *
 *  - 「自建」= 无 `subscriptionId`；「组网」= 按节点能力（`isMeshNode`）派生；
 *    「各订阅」= `subscriptionId` 指向该订阅。三者全是**派生**属性，没有一个可自由改写的 group 字段。
 *  - 唯一在结构上可写的是 `subscriptionId`，但把自建节点写进某订阅是**数据丢失**操作：
 *    `subscription.rs::reconcile`（:755）按 `subscriptionId` 分区后用拉取结果整体替换该分区
 *    → 该节点在下次订阅刷新时被当作"已下架"删掉。
 *  - 反方向（订阅节点 → 自建）在 UI 上不可达：批选按钮在订阅 tab 被隐藏
 *    （原型 `syncNodeToolbar` 的 `nt-hide-sub`），批选态下不存在带 `subscriptionId` 的节点。
 *  - 上游侧全仓无 move-to-group（`grep moveToGroup` 零命中）——原型那个 `batch-move`
 *    是 `notify('已移动')` 的纯 mock，没有对应真实能力。
 *
 * 故按项目红线**如实置灰 + 诚实文案**，而不是接一个会吃掉用户节点的假功能。
 * 若将来引入真正的用户分组字段（`ServerConfig.groupId` + 后端 `server_move_to_group`），
 * 本函数返回非空即自动解禁按钮。
 */
export function moveToGroupTargets(): { id: string; label: string }[] {
  return [];
}

/** 批量「移动到分组」是否可用（= 存在可选目标分组）。 */
export function canMoveToGroup(): boolean {
  return moveToGroupTargets().length > 0;
}

/* ────────────────────────────────────────────────────────────────────────────
 * #5 WARP 管理菜单
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * WARP 已注册后「管理」菜单的条目 id（原型 :5003 `meshWarpManage`）。
 *
 * **未移植原型的「取消 WARP+ 授权」**（诚实边界，非遗漏）：
 *  - 原型那一项是纯本地 mock（`meshState.warpPlan='free'; renderMesh()`），不发任何请求；
 *  - Cloudflare 的设备 API 只有"应用许可"（`warp_apply_license` → `PUT /reg/{id}/account`），
 *    **没有撤销许可的端点**——降级回免费版的真实做法就是重新注册一台匿名设备（= 本菜单的
 *    「重新注册」，注册时选「免费 WARP」）；
 *  - 且节点上根本没有持久化 plan（`doRegister` 只落 `warpDevice`，`meta.warpPlus` 不入库），
 *    连"当前是不是 WARP+"都无从判断——渲染一个 plan 条件项就是在猜。
 * 与其挂一个永远灰着或点了只改本地状态的假开关，不如不挂：升级/更换许可走「编辑设置」，
 * 降级走「重新注册」，两条都是真链路。
 */
export type WarpMenuItem = 'edit' | 're-register' | 'deregister';

/**
 * WARP 卡片点击后应有的行为。已注册 → 弹管理菜单；未注册 → 直接进注册向导（原型同形）。
 * 抽成函数是为了让"已注册时**不再**直接开编辑弹窗"这条行为变更可被断言。
 */
export function warpCardAction(hasWarpNode: boolean): 'menu' | 'register' {
  return hasWarpNode ? 'menu' : 'register';
}

/* ────────────────────────────────────────────────────────────────────────────
 * 订阅「更多」菜单
 * ──────────────────────────────────────────────────────────────────────────── */

/** 订阅「更多」菜单条目（原型 :4778 `subMenu`）。 */
export type SubMenuItem = 'rename' | 'edit-url' | 'copy-url' | 'interval' | 'delete';

/**
 * 「删除订阅」项的文案参数：原型逐字带上被连带移除的节点数
 * （`删除订阅（移除 48 个节点）`）——删订阅会连删其下全部节点
 * （`subscription.rs::apply_subscription_delete` 的 `retain`），不报数用户不知道代价。
 */
export function subDeleteNodeCount(
  servers: readonly Pick<ServerConfig, 'subscriptionId'>[],
  sub: Pick<SubscriptionConfig, 'id'>,
): number {
  return servers.filter((s) => s.subscriptionId === sub.id).length;
}

/* ────────────────────────────────────────────────────────────────────────────
 * 订阅流量用量条
 * ──────────────────────────────────────────────────────────────────────────── */

/** 用量条告警阈值（百分比）。契约「订阅栏显流量用量条(≥85%warn)」= 上游 `server-page.tsx`。 */
export const SUB_USAGE_WARN_PCT = 85;

/**
 * 订阅用量条派生值（已用字节 / 总量 / 百分比 / 是否告警）。
 *
 * 提到 logic 层而非留在 `SubInfoBar` 里内联：阈值是**契约数字**，必须能被单测钉住
 * （此前内联写成 80，比契约早一档变红，且没有任何测试会发现）。
 * `total<=0`（机场不下发总量）→ 百分比 0、不告警，调用方据 `total>0` 决定是否渲染。
 */
export function subUsage(
  userInfo: SubscriptionConfig['userInfo'] | undefined,
): { used: number; total: number; pct: number; warn: boolean } {
  const used = (userInfo?.upload ?? 0) + (userInfo?.download ?? 0);
  const total = userInfo?.total ?? 0;
  const pct = total > 0 ? Math.min(100, (used / total) * 100) : 0;
  return { used, total, pct, warn: pct >= SUB_USAGE_WARN_PCT };
}

/* ────────────────────────────────────────────────────────────────────────────
 * 节点卡「设为出口」的三态判定
 * ──────────────────────────────────────────────────────────────────────────── */

/** 点「设为出口」（整卡或卡上按钮）该做什么。 */
export type NodeUseAction =
  /** 已是当前出口 —— 后端重选同一节点是空操作，弹「已切换」是噪声。 */
  | 'noop'
  /** 直接切换，不收确认税（显式按钮 + 不重启内核那档）。 */
  | 'switch'
  /** 武装二次确认（**普通切换**：整卡是个大命中面，误点代价虽轻但发生率高）。 */
  | 'confirm'
  /** 武装二次确认（**会重启内核**：该节点在待应用差集里，选中它会断开现有连接）。 */
  | 'confirm-restart';

/**
 * 触发面。两个面的**误触概率差一个量级**，故确认策略不同（陈先生 2026-07-30 裁定）：
 * - `card`：整张卡，列表里最大的命中面，滑动/误触即中
 * - `button`：`.nd-acts` 里那颗显式按钮，命中面小、意图明确
 */
export type NodeUseVia = 'card' | 'button';

/**
 * 提到 logic 层而非留在 `NodesScreen` 的 `useCallback` 里：这是**判据**不是接线，
 * 而判据必须能被单测钉住。留在组件里就只能靠人眼复核「哪种情况确认、哪种不确认」，
 * 而本仓 vitest 是 `environment:'node'`（无 jsdom），组件回调根本跑不到。
 *
 * `willRestart` 复用 `willRestartOnSelect` 的谓词、由调用方传入布尔 —— 本函数不自己读差集字段，
 * 否则同一条「哪些节点选中会重启」的判据在仓里会有两份。
 *
 * # 确认策略按触发面分档（陈先生 2026-07-30 两次裁定的合并结果）
 *
 * 判据是**误触概率 × 错误状态能存活多久**，不是「操作可不可逆」：
 * - `card` 一律确认 —— 整卡是列表里最大的命中面，滑动即中，而误切**当场改变全局出口**，
 *   用户可能过很久才发现自己在用错误的节点出网。可逆不等于无代价。
 * - `button` 直切 —— 命中面小、语义明确（按钮上就写着「设为出口」），点它就是要切，
 *   给它加确认税是拿高频动作换一个几乎不发生的误触。
 * - **重启那档两个面都确认** —— 代价不再是「视觉变了、再点回来」，而是断掉现有连接，
 *   这条信息必须在动作前给到，与命中面大小无关。
 */
export function nodeUseAction(
  serverId: string,
  selectedServerId: string | null | undefined,
  willRestart: boolean,
  via: NodeUseVia,
): NodeUseAction {
  if (serverId === selectedServerId) return 'noop';
  if (willRestart) return 'confirm-restart';
  return via === 'button' ? 'switch' : 'confirm';
}
