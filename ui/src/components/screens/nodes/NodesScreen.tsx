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

import { useEffect, useLayoutEffect, useMemo, useRef, useState, useCallback } from 'react';
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
  latencySortSelector,
} from './nodes-logic';
import { useScrollBatch } from '@/lib/use-scroll-batch';

type SortKey = 'default' | 'name' | 'lat' | 'proto';

/** 批量删除按钮的原地二次确认 key（单节点删除用 `node-del:<id>`，见 requestDelete）。 */
const BATCH_DEL_KEY = 'batch-del';

/**
 * SSR 安全的 layout effect。补批要在 paint **之前**收敛（见 topUpBatch 下方那条 effect 的判据），
 * 但 `renderToStaticMarkup` 下 `useLayoutEffect` 会打 React 警告，而本屏的首帧真渲染门
 * （`nodes-render-budget.test.tsx` 门 4）正跑在 node 里。
 *
 * 判据用 `window` 而**不是** `document`：本仓多个 node 环境测试会给 `globalThis.document` 垫桩
 * （i18n 在模块加载期就要写 `<html lang>`），拿 document 判会在那些测试里错误地选到 layout 分支。
 */
const useIsomorphicLayoutEffect = typeof window === 'undefined' ? useEffect : useLayoutEffect;

/**
 * 分批监听挂不上时的**运行期自曝**（开发者可见日志，非用户文案）。
 *
 * `nodes-render-budget.test.tsx` 的锚点门只覆盖**一个**向量：`.main-scroll` 这个类名被改掉。
 * 另一个向量它覆盖不到 —— 网格在运行期不是该容器的后代（AppShell 换层级、本屏被别处复用、
 * 渲染进 portal）：两个类名都还在、门照绿，而 `closest` 返回 null。故那一路由代码本身 fail-open
 * 兜底（见 topUpBatch），再由这条日志把「兜底真的被走到了」说出来。
 * 一次会话报一次：fail-open 之后每次提交都会再走一遍这条路径，逐次打印只会淹没控制台。
 */
let missingScrollerWarned = false;
function warnMissingScroller(): void {
  if (missingScrollerWarned) return;
  missingScrollerWarned = true;
  console.warn(
    '[NodesScreen] 找不到 .main-scroll 滚动祖先：节点网格的滚动分批已 fail-open（本次一次性渲染全部节点）。'
  );
}

/* ════════════════════════════════════════════════════════════════════════════
 * 为什么补批**放弃枚举触发面、改成观测结果**（2026-08-16 复审结论，留档）
 * ════════════════════════════════════════════════════════════════════════════
 *
 * 补批收敛于「距底 > `SCROLL_BATCH_AHEAD_PX`(240px)」。曾经的做法是：把「能改变网格高度」的向量
 * 逐条枚举进 layout effect 的依赖数组，并论证其余向量的幅度都 < 240px 余量（滚动条还在 ⇒ 用户
 * 一滚就续上 ⇒ 至多「多滚一次」，不是永久卡死）。**这条路是错的**，两处实证：
 *
 * ① **枚举读不到过渡中的几何**。`.side` 带 `transition:width .3s ease-out`（components.css:717）。
 *    `sidebarCollapsed` 翻转那一次，layout effect 在 commit 后立刻量 —— 此刻过渡 progress=0，
 *    `.side` 的 used value 还是**旧宽度**，`.main{flex:1}` ⇒ 主区宽还是旧的 ⇒ `auto-fill` 列数
 *    还是旧的 ⇒ 量到的是折叠**前**那份更高的内容，判「仍溢出」⇒ 不补批。300ms 后列数 4→5、
 *    行数 15→12、内容矮 3×141 + 3×12(gap) ≈ 459px ⇒ 不再溢出 ⇒ 卡死。把键换成折叠钮，缺陷与它
 *    本要修的那条逐字同型。
 *    （展开是安全侧；坏的是折叠这一侧。）
 *
 * ② **枚举漏得掉，而且漏的是最高频的那颗**。原表把「角标增减」判成「量级仍是一行文本」——
 *    读反了 `grid-auto-rows:1fr`：它的效果是**所有行等于全局最高卡**（prototype.css:679 自己的
 *    注释就写着「stretch+auto-rows:1fr=全卡跨行统一等高(全局最高卡)」），故总幅度 = **行数 × Δ**。
 *    k=60、N=4 ⇒ 15 行，一排 chip ≈ 20~25px ⇒ 300~375px，**直接跨过 240px 余量**。
 *    这行结论今天侥幸成立，靠的是 `.nd-card{min-height:141px}`（screens.css:117）这个地板把一排
 *    chip 的自然高吃掉了 —— 而地板只在自然高 < 141px 时有效，原表从头到尾没提过它。
 *    更要命的是漏项：`selectedServerId`（点卡设为出口，本屏最高频动作）会把 `.nd-cur` chip 从 A 卡
 *    挪到 B 卡；A 卡若原本是唯一最高卡、且正因这颗 chip 排到 3 行，掉回 2 行 ⇒ 15 行各矮 ~23px
 *    ⇒ 内容矮 345px。它不改 `renderCount`、不改 `visibleServers.length`、不发 resize。
 *    同型的还有 `stagedOnly` / `invalidReason` / `shadowedCidrs`。
 *    **一张列到 12 行的表仍漏了它** —— 每加一颗角标就多一条要枚举的边，而漏一条的后果是
 *    「用户永远点不到剩下的节点且看不出少了」。这就是改用观测的理由。
 *
 * 现在是**三个采样器**，各自观测结果、不枚举原因。三条合起来才叫「不枚举」，缺任一条都有洞：
 *
 *  ① **每次 commit 一采**（layout effect 无依赖数组）—— 覆盖**瞬时**内容变化：增删节点、搜索/
 *     筛选、切 tab、`selectedServerId` 换出口、`stagedOnly`/`invalidReason`/`shadowedCidrs` 角标
 *     增减、语言切换。这些不带 CSS 过渡，commit 那一刻量到的就是真值，一次采样够。
 *  ② **`ResizeObserver` on `.main-scroll`** —— 覆盖**容器盒子**变化：窗口 resize / 缩放 / 全屏、
 *     侧栏折叠的整个 300ms 过渡（连续回报）、差集条与更新横幅进出、滚动条出现/消失。
 *     它取代了原来的 `window.addEventListener('resize')`：今天所有改窗口几何的路径都落在这个盒子上，
 *     旧监听是它的子集。（**不写「必然」**：Windows per-monitor DPI 迁移会给出等逻辑尺寸的新 rect，
 *     CSS px 不变、RO 不发；那一档也不改 auto-fill 列数，无害，但断言不能过强。）
 *  ③ **委派 `transitionend` on `.main-scroll`** —— 覆盖**带 CSS 过渡的内容高度**变化，纯防御性保留。
 *     **2026-08-17 更新**：曾经在用的那一条（视图档切换的几何变化）已被收窄掉。`.nd-card` 的
 *     transition 原是无 property 限定的简写 `.14s`（⇒ `transition-property:all`），列表档把
 *     `min-height`/`padding`/`border-width`（screens.css:364-365）一并改掉 ⇒ 切档那一次，①量到的
 *     是**过渡前**的卡高，而②只看容器盒子、内容变矮不改它 —— 那一维在两者之间是个洞，与侧栏那条
 *     High 同型。现在 `.nd-card` 的 transition 已收窄到六个「不参与盒模型」的绘制层属性（完整清单
 *     见 screens.css:61 注释），几何属性随切档瞬切 ⇒ scrollHeight 单次采样即是终值，`view` 这一维
 *     不再需要③补齐（但 border-color/background/box-shadow/outline/border-radius/outline-offset
 *     仍按 140ms 渐变，是混合过渡不是纯瞬切——只是不影响几何采样，见 screens.css:61 的详细说明）。
 *     **仍不删的理由**：③守的是「`.main-scroll` 内任何带 CSS 过渡的内容高度变化」这整条机制，不是
 *     `view` 这一个案例——给 `.nd-card` 或本屏其它元素今后新加任何动画化几何属性的样式，它立刻
 *     重新变成非它不可。删掉它等于把「今天没有已知触发案例」误判成「以后也用不到」，是本屏刚放弃
 *     的「枚举触发面」思路（见上方「为什么补批放弃枚举触发面」整段）从「布局向量」换个马甲搬回
 *     「过渡属性」。
 *     **成本如实记账（别被「view 已不触发」误读成③几乎不响）**：③今天的主触发源不是 view 切换，
 *     是**悬停**——`.nd-card:hover`（border-color/box-shadow）、`.nd-a:hover`、`.proto-chip:hover`
 *     等一切冒泡到 `.main-scroll` 的 `transitionend` 都会进 `topUpBatch` 读一次 `scrollHeight`（强制
 *     一次布局），鼠标划过节点网格时相当高频。这在收窄前后同频、不是本次改动引入的回归，只是
 *     narrowing 前容易被「反正 view 才是大头」的直觉带偏——它一直是次要开销，成本极低（一次强制
 *     布局读 + `advanceBatch` 同值 bail-out），不值得为省它把③整条撤掉。
 *
 * **已收窄（2026-08-17，陈先生拍板执行；同日复审 F1/F2/F5/F7/F8 追加修正，一并留档）**：`.nd-card`
 * 的 `transition:.14s` 曾写成无 property 限定的简写（= `all`）。判据是「不参与盒模型的绘制层属性
 * 才收进来」，不是「只列 hover 用到的那几个」——首版漏了 `border-radius`（符合判据却被漏收）、
 * 且被 `outline` 简写不含的 `outline-offset` 摆了一道，复审后按判据重扫，收窄到六个属性：
 * `border-color`/`box-shadow`/`background`/`border-radius`/`outline`/`outline-offset`
 * （screens.css:114-117 + index.css 的收窄覆盖层——完整的选择器核对清单、`gap` 为何刻意不收进来，
 * 都写在 screens.css:61 的姊妹注释里，此处不复述）。`screens.css` 那份声明单独存在不生效：
 * `prototype.css:682` 有逐字同选择器、同特异性的未收窄声明，且是 `index.css` 的最后一个 `@import`
 * ⇒ 同特异性后者胜，真正生效的是 index.css 覆盖层；两处 transition 取值必须逐字相等，
 * `style-invariants.test.ts` 有专门的门钉住。
 *
 * **F1（真实缺陷，已随本次收窄一并修）**：列表档 `border:0`/`border-bottom:0`（screens.css:365/366）
 * 是简写，未提到的 border-*-color 会复位成 `currentcolor`（`.nd-card` 继承 body 的 `--fg`）。收窄让
 * `border-width` 瞬切、`border-color` 仍按 140ms 渐变 ⇒ 若不管，会在「切档」以及**更高频的**「滚动
 * 分批推进/搜索击键导致哪张卡是 `:last-child` 改变」两条路径上闪一道近黑/近白边框再淡到
 * `--line`/`--hair`。已在 index.css 覆盖层里把 border-color 显式钉死在 `--line`/`--hair`、两态都
 * 不含 currentcolor（完整推导见 index.css 里紧邻 `.nd-card.confirming` 的那条注释）。
 *
 * 六个属性均不参与盒模型，故视图档切换的几何变化（`min-height`/`padding`/`border-width`/`gap`/
 * `flex-direction`）随之变回瞬切 ⇒ 采样器③对 `view` 这一维随之变成冗余（仍保留，见上方该采样器
 * 条目——它守的是机制，不是这一个案例）。**但这不是纯瞬切**：上面六个绘制层属性仍按 140ms 渐变，
 * 只是它们不影响 `scrollHeight`，故不改变采样器③已冗余这个结论——是「几何瞬切 + 颜色/圆角/描边
 * 仍渐变」的混合过渡，向量表那一行按此口径记账，别读成「整条规则不再有过渡」。
 * **用户可见的取舍（如实记）**：卡片↔列表切换的**几何**（尺寸/位置）从渐变 morph 变成瞬切，颜色/
 * 圆角/描边仍柔化过渡。纯几何渐变的代价是 60+ 张卡的 `min-height` 逐帧参与布局重算（本屏这份分批
 * 基础设施的存在理由之一）；几何瞬切换回来的是零几何动画开销、且从根上消掉「过渡中途采样陈旧几何」
 * 这整类缺陷的触发面。
 *
 * 下表是**留档**，不再是判据来源（像素取自各自 CSS 盒模型，量级判定非精确测绘）：
 *
 * | 向量 | 改的是 | 幅度 | 曾经的判定 | 现在由谁收 |
 * |---|---|---|---|---|
 * | 视图档 `view`（卡片↔列表，**2026-08-17 起几何瞬切、颜色/圆角/描边仍 140ms 渐变**，见上方「已收窄」段） | 几何（min-height/padding/border-width/gap/flex-direction） | 行高 141px ↔ ~40px；60 条差 2~3 倍 scrollHeight | 收窄前：会卡死 → 进依赖（`transition:all`，③ transitionend 补） | ① 每次 commit（几何瞬切，一采即真值——scrollHeight 不吃仍在渐变的颜色/圆角/描边；③ 仍挂着，对这一维是无害空转，见上方③条目「成本如实记账」） |
 * | 侧栏折叠 | 主区宽 ±92px / mac ±68px → 列数 ±1 | 窄窗 N=4、k=60 时 15→12 行 = 3×141 + 3×12(gap) ≈ 459px | 会卡死 → 进依赖（**读到的是旧几何，实际没收住**） | ② RO（过渡全程连续回报） |
 * | 窗口 resize / 缩放 / 全屏 | 可视区 + 内容 | 无界 | `window resize` 监听 | ② RO（旧监听已撤；per-monitor DPI 迁移那一档 RO 不发，见上） |
 * | **出口切换 `selectedServerId`** | 内容（`.nd-cur` chip 换卡 → 最高卡行数变 → **每行**跟着变） | 15 行 × ~23px = 345px | **整条漏列** | ① 每次 commit（无过渡，一采即真值） |
 * | `stagedOnly` / `invalidReason` / `shadowedCidrs` 角标增减 | 同上 | 行数 × ~20~25px，今天靠 141px 地板吃掉 | 判成「一行文本」（读反 1fr） | ① 每次 commit |
 * | 语言切换 / 字体换装 | 同上 | 同上 | 同上 | ① 每次 commit |
 * | 搜索 / 协议筛选 / 切 tab | 内容 + 结果集身份 | — | resetKey 复位 + 长度变 | ① 每次 commit |
 * | 增删节点 / 订阅刷新 / staged 变更 | 内容 | — | 长度变 | ① 每次 commit |
 * | 批选条进出（`.batch-bar`） | 内容 ±~62px（padding 20 + border 2 + margin-bottom 14 + 行高） | < 240 | 余量兜底 | ① 每次 commit |
 * | PendingChangesBar 出现/消失 | **可视区** ±36px（`.pending-bar.show`；在 `.main-scroll` 之外） | < 240 | 余量兜底 | ② RO |
 * | AppUpdateBanner 出现/消失 | **可视区** ±~74px（同上，在 `.main-scroll` 之外） | < 240 | 余量兜底 | ② RO |
 * | SubInfoBar 出现/消失 | 内容 ±~80px | < 240 | 余量兜底 | ① 每次 commit |
 * | 延迟数值回填 | 内容：`.nd-lat` 是**条件渲染**（NodeCard:274），盒高 11px×1.5+4=20.5px > `.nd-name` 的 18.9px 行盒 ⇒ 首个结果到达时 `.nd-top` 涨 ~1.6px，再经 1fr 摊到每行 | ~1.6px × 行数 | 判成「0」（**当时写的是断言不是测量**） | ① 每次 commit |
 *
 * 两处「结论成立但理由写错了」的更正（Low-2，一并留档）：
 *  · 批选条不改卡高。旧理由（「`.nd-check` 是 absolute」）**对卡片档成立**（screens.css:160），
 *    但只覆盖卡片档 —— 列表档它是 `position:static; order:-1`（:375）在流内。列表档成立的理由是
 *    行高由 `.nd-acts` 里 27px 的 `.nd-a` 撑住，18~21px 的勾选框够不着。
 *  · 「延迟数值回填零高度变化」不成立，见上表该行。方向安全、量级也小，但那是断言不是测量值。
 * ════════════════════════════════════════════════════════════════════════════ */

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
              })
            : t('nodes.useConfirmToast', {
                node: server.name,
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
  /**
   * 「被覆盖」角标的**成品**（byId 已解析成显示名），按节点 id 索引。
   *
   * 为什么要 memo 到这一步、而不是在 `.map()` 里就地 `?.map(...)`：那样每次父层渲染都会给
   * 每个有冲突的节点造一个新数组 ⇒ `shadowedCidrs` 这个 prop 每次都判不等 ⇒ 这些卡的 `memo`
   * 恒失效。冲突节点少，但「少数卡片永远不 bail-out」正是最难被发现的那种回归。
   */
  const shadowedNamed = useMemo(() => {
    const named = new Map<string, { cidr: string; by: string }[]>();
    for (const [id, list] of shadowedIndex) {
      named.set(
        id,
        list.map((s) => ({ cidr: s.cidr, by: serverNameById.get(s.byId) ?? s.byId }))
      );
    }
    return named;
  }, [shadowedIndex, serverNameById]);

  // 测速态。结果读**全局 store**、进度走**全局 toast**（两者订阅都在 App.tsx 顶层，切屏不丢）。
  // 本屏只留 `testing` 这一位灰态（按钮禁用），它是本屏控件的属性、天然是组件私有。
  // 勿把延迟改回 useState，也勿把进度订阅搬回来——那正是「切屏即丢」的来源。
  /**
   * 父层**只在按延迟排序时**才订整张表 —— 那时排序结果必须随每次回包同步重排（不变量①），
   * 重渲是必要的；其余三档排序（默认/名称/协议）下父层根本不读延迟，订了就是每轮测速几十上百次
   * 白重渲整片网格。
   *
   * 非 `lat` 档由 `latencySortSelector` 返回**模块级常量哨兵**（`EMPTY_LATENCY_MAP`）。
   * 这一点是本改动成立的全部前提：zustand 默认按 `Object.is` 比较选择器结果，选择器里若写
   * `: {}` 字面量，每次提交都是新对象、判不等，父层照样每次提交重渲 —— 改了等于白改。
   * 判据与真断言见 `nodes-logic.ts` 的 `EMPTY_LATENCY_MAP` 头注 + `nodes-render-budget.test.tsx`。
   *
   * 单卡的延迟不再从这里灌下去：每张 `NodeCard` 按自身 id 细粒度订阅（见该文件头注）。
   * 需要「此刻最新的整张表」的是三条**动作腿**（删/批删/注销 WARP 的兜底出口），它们在点击当刻
   * 用 `useLatencyStore.getState()` 现取，既拿到最新快照，又不制造一条订阅。
   */
  const latencies = useLatencyStore(latencySortSelector(sortKey === 'lat'));
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

  /* ── 节点网格的**渲染尾**分批（复用 `lib/use-scroll-batch`，本仓第三个消费方，零新依赖）──
   *
   * 只切渲染尾，不切数据：`visibleServers` 已是 search / protoFilter 作用后的**完整结果**，
   * 全选、工具栏「测速」（可见集）、批选条三处**必须继续读它**。把切片回灌那三处，就是日志页
   * 「500 行以外搜不到」那类回归的同型复发 —— 用户以为自己在对「筛出来的全部」操作，实际只对
   * 屏幕上恰好画出来的那一批。该不变量由 `nodes-render-budget.test.tsx` 钉住。
   *
   * `resetKey` = 结果集身份（tab + 搜索 + 协议筛选 + 排序键）：一变即回首批，否则从窄结果切回
   * 宽结果时会残留一个大计数，分批等于白做。分隔符用 `\u0000` 免得 `a|b` 与 `a` + `|b` 撞。 */
  const gridRef = useRef<HTMLDivElement>(null);
  const {
    count: renderCount,
    onScroll: onGridScroll,
    renderAll: renderAllServers,
  } = useScrollBatch(
    visibleServers.length,
    `${activeTab}\u0000${search}\u0000${protoFilter}\u0000${sortKey}`
  );
  const renderedServers = useMemo(
    () => visibleServers.slice(0, renderCount),
    [visibleServers, renderCount]
  );
  /* hook 每次渲染都返回新的闭包（它们捕获着当轮的 total）。用 latest-ref 转发，
     使下面那条监听只在挂载/卸载时装拆一次，而不是每渲染一次就重挂一次。
     这条同步**必须**也是 layout 档、且声明在补批 effect 之前：layout effect 整体跑在 passive 之前，
     写成 useEffect 会让补批读到上一轮的闭包（旧 total），节点变多时补批会停在旧上限。 */
  const gridScrollRef = useRef(onGridScroll);
  const renderAllRef = useRef(renderAllServers);
  /** `closest('.main-scroll')` 落空过 —— 由 layout 档的 `topUpBatch` 置位、passive 档的 fail-open 消费。
   *  走 ref 不走 state：置位发生在 layout effect 里，用 state 会多一次同步重渲才轮到兜底。 */
  const scrollerMissingRef = useRef(false);
  useIsomorphicLayoutEffect(() => {
    gridScrollRef.current = onGridScroll;
    renderAllRef.current = renderAllServers;
  });
  /**
   * 追加下一批的触发器。**滚动容器是 `AppShell` 的 `.main-scroll`**：本屏是普通 `.screen`
   * （`flex:1 0 auto; display:block`），自己不滚，整页滚在那个祖先上。scroll 事件不冒泡、
   * React 的 `onScroll` 也不做委派，故这里必须找到那个祖先自己挂原生监听 —— 把 `onScroll`
   * 挂在 `.node-grid` 上是一行不会报错、也永远不会触发的死代码。
   *
   * 找不到祖先时**不**在这里兜底，只置位 `scrollerMissingRef`：`topUpBatch` 跑在 layout 档，
   * 而兜底动作是「一次取到底」，压在 paint 之前就是数秒白屏。真正的兜底在下方那条**独立的
   * passive effect** 里（判据写在它自己的头注上）。
   */
  const topUpBatch = useCallback(() => {
    const scroller = gridRef.current?.closest<HTMLElement>('.main-scroll');
    if (scroller) gridScrollRef.current({ currentTarget: scroller });
    else scrollerMissingRef.current = true;
  }, []);
  useEffect(() => {
    const scroller = gridRef.current?.closest<HTMLElement>('.main-scroll');
    if (!scroller) {
      scrollerMissingRef.current = true;
      return;
    }
    scroller.addEventListener('scroll', topUpBatch, { passive: true });
    /* 采样器②：**容器盒子**。观测 `.main-scroll` 自身，覆盖窗口/侧栏过渡/缩放这一维。
       同款用法本仓已有先例：`home/ConnectionTopology.tsx:154` 观测 `.sankey`。

       **不带 `box` 参数**（默认 content-box，别改）：`.main-scroll` 是 `overflow-y:auto`，
       Win/Linux 经典滚动条下内容从「不溢出」跨到「溢出」会让 content-box 宽缩约 15px ——
       那一维**恰恰要**观测（网格变窄 ⇒ auto-fill 列数变 ⇒ 行数变）。改成 `border-box`
       会把它整条丢掉，而全部门仍绿。

       它**确实**构成一条回调 → 自身盒子变化 → 再回调的反馈边（就是上面那 15px）。不自激的理由
       不是「盒子不变」，而是 `count` 单调不减 + `total` 封顶 + `advanceBatch` 按值 bail-out
       （同值 ⇒ React 就地停），外加 `shouldAdvance` 的 `clientHeight <= 0` 短路。 */
    const ro = new ResizeObserver(topUpBatch);
    ro.observe(scroller);
    /* 采样器③：**带 CSS 过渡的内容高度变化**，纯防御性保留（2026-08-17 更新，详见文件头注）。
       `transitionend` 冒泡，故一条委派监听即可。

       曾经非它不可的那个案例已被消掉：`.nd-card` 原写的是 `transition:.14s`（简写、无 property
       限定 ⇒ `transition-property:all`），列表档把 `min-height`/`padding`/`border-width`
       （screens.css:364-365）一并改掉 ⇒ 切视图档那一次，commit 后立刻量到的是**过渡前**的卡高。
       现在 `.nd-card` 的 transition 已收窄到六个不参与盒模型的绘制层属性（border-color/box-shadow/
       background/border-radius/outline/outline-offset，完整清单见 screens.css:61）⇒ 几何属性随
       切档瞬切，采样器①单次采样即是终值——**但这六个属性本身仍按 140ms 渐变**，不是整条规则变
       瞬切，只是它们不影响 scrollHeight，不改变「①单次采样即真值」这个结论。

       **仍不删的理由**：这条守的是「`.main-scroll` 内任何带 CSS 过渡的内容高度变化」这整条机制，
       不是 `view` 这一个案例——给 `.nd-card` 或本屏其它元素今后新加任何动画化几何属性的样式，它
       立刻重新变成非它不可，删掉等于把「今天没有已知触发案例」误判成「以后也用不到」。

       成本如实记账（别被「view 已不触发」带偏，以为③今天几乎不响）：③今天的主触发源是**悬停**——
       `.nd-card:hover`（border-color/box-shadow）、`.nd-a:hover`、`.proto-chip:hover` 等一切
       冒泡到 `.main-scroll` 的 `transitionend` 都会进这里读一次 `scrollHeight`（强制一次布局），
       鼠标划过节点网格时相当高频；card↔list 切换本身也仍会触发（六个绘制层属性在两档间的值确有
       不同，只是不再影响几何）。这些事件读到的值和采样器①已经一致，`advanceBatch` 同值
       bail-out，是无副作用的空转，成本是一次强制布局读，不是错误重算，也不是本次改动引入的新
       开销（悬停触发在收窄前后同频）。不做 propertyName 过滤：过滤就是又一张要维护的枚举表，
       正是本屏刚放弃的那条路。 */
    scroller.addEventListener('transitionend', topUpBatch);
    return () => {
      scroller.removeEventListener('scroll', topUpBatch);
      scroller.removeEventListener('transitionend', topUpBatch);
      ro.disconnect();
    };
  }, [topUpBatch]);
  /**
   * `closest` 落空时的 **fail-open**：一次取到底，宁可多画，不许静默只剩首批（用户没有滚动条、
   * 没有任何「还有更多」的暗示，看不出少了，而全选/测速读的仍是全集 ⇒ 界面与操作对象脱节）。
   *
   * 刻意留在 **passive** 档、且刻意不设上限：
   *  · passive —— `renderAll()` 会把 count 顶到 total，几千张卡（本仓每卡 ≥22 个 DOM 元素、
   *    含 10 处内联 SVG）的 VDOM+DOM 若压在 paint 之前，用户连「至少有 60 个」都看不到，
   *    「少显示节点」被换成「窗口冻住数秒」。放 passive 档 = 先让首批画出来，再补齐剩下的。
   *  · 不设上限 —— 上限（如 600）会把这条兜底重新变成一个静默截断，而它存在的全部理由正是
   *    消灭静默截断。这条路径意味着 AppShell 的结构已经坏了，一次长帧是诚实的代价。
   * 无依赖数组：结果集变大时要再顶一次；已到顶时 `renderAll` 返回同值 ⇒ React 就地 bail-out。
   */
  useEffect(() => {
    if (!scrollerMissingRef.current) return;
    warnMissingScroller();
    renderAllRef.current();
  });
  /**
   * **初批必须覆盖视口**，否则整个分批是个陷阱：内容没撑出滚动条 ⇒ 永远收不到 scroll 事件 ⇒
   * 剩下的节点再也出不来（而用户看不出少了 —— 没有滚动条就没有「还有更多」的暗示）。
   * 每次提交后按同一条判据（距底不足 `SCROLL_BATCH_AHEAD_PX` 就再来一批）补批，收敛于
   * 「撑出滚动条」或「取完」两者之一：`advanceBatch` 在不该推进或 `c >= total` 时返回同一个
   * count，React 就地 bail-out，不会自激。
   *
   * 走 **layout** 档而不是 `useEffect`：4K / 超宽下首屏容量远大于每批 60，收敛要连跑三四轮，
   * 而 passive effect 每轮之间都夹着一次真实绘制 ⇒ 用户看见网格分段自下而上长出来（2026-08-16
   * 复审报的量级：约 50~65ms 可见抖动）。layout 档把收敛循环整个压在 paint 之前跑完。
   *
   * **没有依赖数组**（= 文件顶部的**采样器①**）—— 每次提交都重量一次，覆盖**瞬时**内容变化。
   * 它单独并不够：带 CSS 过渡的高度变化在这一刻量到的还是过渡前的值（那一维归采样器③
   * `transitionend`），容器盒子变化根本不经过本屏 commit（归采样器② RO）。三条的分工见文件顶部。
   * 代价如实记账：
   *  · 每次本屏 commit 多一次强制 layout 读（`scrollHeight`/`clientHeight`）。按延迟排序的一轮
   *    测速里父层每个回包都重渲 ⇒ 约 200 次；但那些 layout 本来在 paint 时也要做，这里只是提前，
   *    不是凭空多出一遍（真正被消掉的重渲在别处：非 `lat` 档的条件订阅 + 单卡 memo）。
   *  · 收敛循环本身：每轮一次 `slice` + 一批卡片的 VDOM，换的是「首帧即定稿」。
   */
  useIsomorphicLayoutEffect(topUpBatch);

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
        toast.info(t('nodes.noTestableNodes'));
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
        absorbRunResult(await api.server.speedTest([server.id]));
      } catch (err) {
        speedTestError(err, 'speedTest one');
      }
    },
    [absorbRunResult, speedTestError]
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
        toast.error(t('nodes.copyLinkUnsupported'));
        return;
      }
      // 剪贴板失败与「协议无分享链接」是两回事：原先同一个 catch 兜住两者，把「链接生成好了但没写进剪贴板」
      // 谎报成「该协议不支持分享」——用户据此以为协议不支持，实际重试即可。批量版 copyLinksBatch 本就分段，此处对齐。
      try {
        await navigator.clipboard.writeText(url);
        toast.success(t('nodes.copyLinkOk'));
      } catch (err) {
        console.error('[NodesScreen] copy link to clipboard failed:', err);
        toast.error(t('nodes.copyLinksFailed'));
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
            ? t('nodes.cloneWarpSingleton')
            : t('nodes.cloneTsSingleton')
        );
        return;
      }
      const { id, subscriptionId, providerName, ...rest } = server;
      const cloneName = t('nodes.cloneName', { name: server.name });
      try {
        // 配置暂存闸门（与 NodeDialog 同形）。克隆 = 造一个新 `servers` 元素，无任何副作用 ⇒ 默认腿。
        // **删除**那几条腿不同：`server_delete` 会把 WARP 设备推进远端注销队列、清 TS state（W-3），
        // 且连带重选 `selectedServerId`（W-1），故它们留在直写腿上。
        if (editRoute('servers', stagingEnabled) === 'staged') {
          const entityId = crypto.randomUUID();
          stage({
            id: `server:${entityId}`,
            kind: 'server',
            label: `${t('node.addTitle')} ${cloneName}`,
            entityPath: ['servers', entityId],
            nextValue: { ...rest, id: entityId, name: cloneName },
          });
        } else {
          await api.server.add({ ...rest, name: cloneName });
        }
        // 原型 :4514 克隆成功即 notify('已克隆节点','ok')。副本落在**自建**分组（上方剥了 subscriptionId），
        // 当前若停在订阅 tab，新卡不在本 tab 可见 → 无 toast 时点了完全没反应，比原型更需要这条反馈。
        toast.success(t('nodes.cloneSuccess'));
      } catch (err) {
        console.error('[NodesScreen] clone failed:', err);
        toast.error(t('nodes.cloneFail'));
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
        toast.info(t('home.stagedOnlyBlocked'));
        return;
      }
      try {
        await api.server.tailscaleLogout(node.id);
        setTailscaleLoginState(node.id, false);
        toast.success(t('nodes.meshTsLogoutOk'));
      } catch (err) {
        console.error('[NodesScreen] tailscale logout failed:', err);
        toast.error(t('nodes.meshTsLogoutFail'));
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
          title: t('nodes.subDeleteTitle'),
          message: t('nodes.subDeleteConfirm', {
            name: sub.name,
            count,
          }),
          confirmLabel: t('common.delete'),
          danger: true,
          onConfirm: () => {
            closeDialog(); // pop confirm
            void api.subscription
              .delete(sub.id)
              .then(() =>
                toast.info(
                  t('nodes.subDeleteOk', {
                    count,
                  }),
                ),
              )
              .catch((err) => {
                // 删失败时订阅 tab 原地不动，与「按钮没反应」不可区分 → 必须透出后端真实原因。
                console.error('[NodesScreen] sub delete:', err);
                toast.error(
                  t('nodes.deleteFail'),
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
            .then(() => toast.success(t('nodes.subCopyUrlOk')))
            .catch((err) => {
              console.error('[NodesScreen] copy sub url failed:', err);
              toast.error(t('nodes.copyLinksFailed'));
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

  /* 卡上「编辑」。此前是调用点的内联箭头 —— 那一个箭头就足以让**每张**卡的 `memo` 恒失效
     （props 浅比较里恒有一个新函数引用），memo 反而只剩比较开销。卡片的其余回调
     （测速/复制/克隆/设为出口/删除/勾选）本来就都是 useCallback，只差这一个。 */
  const editNode = useCallback(
    (server: ServerConfig) => openDialog(editDialogFor(server)),
    [openDialog]
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
          toast.info(t('nodes.deleteSuccess'));
          return;
        }
        void api.server
          .delete(
            server.id,
            // D4：兜底出口须是**剩余**节点里最快的。原先传 selectedServerId（=被删节点自身）→ 后端 viable
            // 校验恒假 → 恒落直连哨兵，删当前节点即静默裸奔。
            // 延迟表在**点击当刻**现取（`getState()`）：本屏已不再无条件订整张表，闭包里捕获的会是
            // 上一次父层渲染时的陈旧快照 —— 用户刚测完速就删节点，兜底会选到过期的「最快」。
            fallbackExitAfterDelete(
              servers,
              selectedServerId,
              new Set([server.id]),
              useLatencyStore.getState().latencyMap
            )
          )
          // 原型 :4140 node-del 成功即 notify('节点已删除')（中性 kind，非 ok）——删除是用户已预期的结果，
          // 报的是「确实删掉了」而非「恭喜」。
          .then(() => toast.info(t('nodes.deleteSuccess')))
          .catch((err) => {
            console.error('[NodesScreen] delete:', err);
            toast.error(t('nodes.deleteFail'));
          });
      });
    },
    [confirmTwice, servers, selectedServerId, stagedOnly, stagedEntries, revertStaged, t]
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
          confirmLabel: t('common.confirm'),
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
                // 同 requestDelete：延迟表点击当刻现取，不吃闭包里的陈旧快照。
                fallbackExitAfterDelete(
                  servers,
                  selectedServerId,
                  new Set([node.id]),
                  useLatencyStore.getState().latencyMap,
                ),
              )
              .then(() => {
                toast.info(opts.okToast);
                opts.afterDelete?.();
              })
              .catch((err) => {
                console.error('[NodesScreen] warp remove failed:', err);
                toast.error(t('nodes.deleteFail'));
              });
          },
        },
      });
    },
    [openDialog, closeDialog, servers, selectedServerId, stagedOnly, stagedEntries, revertStaged, t],
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
          title: t('nodes.meshWarpReRegisterTitle'),
          message: t('nodes.meshWarpReRegisterMsg'),
          okToast: t('nodes.meshWarpReRegisterOk'),
          afterDelete: () => openDialog({ kind: 'warp', edit: false }),
        }),
      onWarpDeregister: (node) =>
        removeWarpNode(node, {
          title: t('nodes.meshWarpDeregisterTitle'),
          message: t('nodes.meshWarpDeregisterMsg'),
          okToast: t('nodes.meshWarpDeregisterOk'),
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
          toast.info(t('nodes.batchDeleteOk', { count: ids.size }));
          return;
        }
        try {
          // 同 D4：兜底出口从**删除集之外**的剩余节点里取最快（后端本批新增 fallback_selected_id 形参，
          // 此前该 key 被 Tauri 静默丢弃、批删掉当前出口恒落直连）。
          // 同 requestDelete：延迟表点击当刻现取，不吃闭包里的陈旧快照。
          await api.server.deleteBatch(
            [...split.backend],
            fallbackExitAfterDelete(
              servers,
              selectedServerId,
              ids,
              useLatencyStore.getState().latencyMap
            )
          );
          exitBatch();
          // 原型 :4137 batch-del 成功即 notify('已删除')（中性）。这里带上条数：批删同时退出批选模式、
          // 选中态一并清空，用户失去「刚才选了几个」的参照，报数才对得上账。
          toast.info(t('nodes.batchDeleteOk', { count: ids.size }));
        } catch (err) {
          console.error('[NodesScreen] batch delete failed:', err);
          toast.error(t('nodes.deleteFail'));
        }
      })();
    });
  }, [selectedIds, confirmTwice, servers, selectedServerId, exitBatch, stagedOnly, stagedEntries, revertStaged, t]);

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
      toast.error(t('nodes.copyLinkUnsupported'));
      return;
    }
    try {
      await navigator.clipboard.writeText(urls.join('\n'));
      toast.success(
        skipped > 0
          ? t('nodes.copyLinksPartial', { count: urls.length, skipped })
          : t('nodes.copyLinksOk', { count: urls.length })
      );
    } catch (err) {
      console.error('[NodesScreen] batch copy links failed:', err);
      toast.error(t('nodes.copyLinksFailed'));
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
          <h1>{t('sidebar.server')}</h1>
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
            <span>{t('nodes.testAll')}</span>
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
              <span>{t('nodes.add')}</span>
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
                  <span>{t('nodes.manualAdd')}</span>
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
                  <span>{t('nodes.meshAddAccess')}</span>
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
                  <span>{t('nodes.manualImport')}</span>
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
                  <span>{t('nodes.addSubscription')}</span>
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
              ? t('nodes.tab.manual')
              : g.isMesh
                ? t('nodes.tab.mesh')
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
        <div className="seg2 nh-view" role="group" aria-label={t('nodes.view.aria')}>
          <button
            type="button"
            className={cn(view === 'cards' && 'on')}
            onClick={() => setView('cards')}
            aria-label={t('nodes.view.cards')}
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
            aria-label={t('nodes.view.list')}
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
            placeholder={t('nodes.search.placeholder')}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </label>

        <Csel
          className="nt-proto"
          id="node-proto-filter"
          ariaLabel={t('nodes.filter.protocol')}
          value={protoFilter}
          onChange={setProtoFilter}
          options={[
            { value: '', label: t('nodes.filter.allProto') },
            ...protoOptions.map((p) => ({ value: p, label: p })),
          ]}
        />

        <Csel
          className="nh-sort"
          id="node-sort"
          ariaLabel={t('nodes.sortBy')}
          value={sortKey}
          onChange={(v) => setSortKey(v as SortKey)}
          options={[
            { value: 'default', label: t('nodes.sort.default') },
            { value: 'name', label: t('nodes.sort.name') },
            { value: 'lat', label: t('nodes.sort.latency') },
            { value: 'proto', label: t('nodes.sort.protocol') },
          ]}
        />

        {/* 测的是搜索/协议筛选之后**你眼前这些**（∩ 可测集），不是整组。
            射程由 `data-tip` 说明承载，不写进按钮字面（陈先生 2026-07-29 裁定）。 */}
        <button
          type="button"
          className="btn ghost sm"
          onClick={testVisible}
          disabled={testing}
          data-tip={t('nodes.testVisibleHint')}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M13 2L4 14h6l-1 8 9-12h-6z" />
          </svg>
          <span>{t('nodes.testVisible')}</span>
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
          <span>{t('nodes.batch')}</span>
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
            aria-label={t('nodes.selectAll')}
          >
            {selectedIds.size === visibleServers.length && visibleServers.length > 0 && (
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
                <path d="M5 12l5 5 9-11" />
              </svg>
            )}
          </button>
          <b>
            {t('nodes.selectedPrefix')}
            <span className="mono">{selectedIds.size}</span>
            {t('nodes.selectedSuffix')}
          </b>
          <div className="sp" />
          <button
            type="button"
            id="batch-test"
            className="btn ghost sm"
            onClick={testSelected}
            disabled={testing || selectedIds.size === 0}
          >
            {t('nodes.testGroup')}
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
              data-tip={t('nodes.batchMoveUnavailable')}
            >
              {t('nodes.batchMove')}
            </button>
          )}
          <button
            type="button"
            className="btn ghost sm"
            onClick={copyLinksBatch}
            disabled={selectedIds.size === 0}
          >
            {t('nodes.batchCopyLinks')}
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
                ? t('nodes.batchDeleteConfirmAgain')
                : t('common.delete')}
            </button>
          )}
          <button type="button" className="btn ghost sm" onClick={exitBatch}>
            {t('nodes.batchExit')}
          </button>
        </div>
      )}
      {/* 节点网格。`ref` 是分批的接线：滚动监听要靠它 `closest('.main-scroll')` 找到真正的
          滚动容器（本屏自己不滚，见 topUpBatch 头注）。 */}
      <div className="node-grid" ref={gridRef}>
        {visibleServers.length === 0 ? (
          <div className="stub" style={{ gridColumn: '1 / -1' }}>
            {/* 空态文案按**组的语义**分流：订阅组的节点由订阅拉取而来，用户在这里点右上「添加」
                只会造出一个自建节点、订阅照旧是空的 —— 该引导他刷新订阅（0 节点订阅也保留 tab，
                见 server-grouping 的 includeEmptyGroups 注释，正是为了留住刷新/删除入口）。 */}
            <p>
              {search || protoFilter
                ? t('nodes.emptyFiltered')
                : activeSub
                  ? t('nodes.emptySub')
                  : activeGroup?.isMesh
                    ? t('nodes.meshEmpty')
                  : t('nodes.empty')}
            </p>
          </div>
        ) : (
          /* 只画渲染尾切片；全选 / 测可见 / 批选条读的仍是完整的 `visibleServers`（见上方判据）。 */
          renderedServers.map((server) => {
            const isMesh = activeGroup?.isMesh || isMeshNode(server);
            // 「仅局域网」角标走 domain 谓词 `meshAllowsInternet`，不再自造
            // `wireguardSettings.allowInternet === false` 弱判定 —— 后者漏掉 Tailscale 这一族
            // （TS 的「有没有外网出口」由 exitNode 派生），无出口的 TS 节点此前既不显角标、⚡ 也照样亮。
            const lanOnly = isMesh && !meshAllowsInternet(server);
            // ⚡ 与延迟位的可测性：与页头/工具栏/批选**同一条**过滤线（`isSpeedTestable`）。
            const blockReason = speedTestBlockReason(server, speedTestCaps, stagedOnly.has(server.id));
            // byId → 显示名（节点可能已被删/不在本 tab；取不到就退回 id，别让 tooltip 变成空引号）。
            // 成品在 `shadowedNamed` 里一次算好：在这里就地 map 会每渲染一次造一个新数组，
            // 使这些卡的 memo 恒失效。
            const shadowed = shadowedNamed.get(server.id);
            return (
              <NodeCard
                key={server.id}
                server={server}
                /* 延迟**不再从这里灌**：每张卡按自身 id 细粒度订阅（NodeCard 头注）。
                   灌 prop 意味着每次逐节点回包都要经父层重渲整片网格才能到达那一张卡。 */
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
                onEdit={editNode}
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
