/**
 * NodeMenu —— 首页出口节点内联选单（1:1 提取自原型 polaris-prototype.html #node-menu，
 * renderNodeMenu/toggleNodeMenu/pickNode/pickDirectExit/toggleNmGroup，L3457-3521）。
 *
 * 原型 DOM（挂载于 HomeScreen 的 `.node-dd`，均为 `#node-menu` 的直接子节点、非嵌套 wrapper）：
 *   .nm-search（服务器数 < 8 时 hidden）> input.input#nm-search-inp + label.nm-sort（延迟排序 .swt）
 *   button.mi（顶部「直连」哨兵，命中显 .nm-ck）
 *   .mm-sep
 *   [button.ns-grp（分组头，chevron + 名称 + 计数）, button.nm-item × N（各节点行）] × 每组
 *   .nm-empty（搜索无命中）
 *   .nm-foot > button「全部测速」+ button「管理节点 →」
 *
 * 数据源：groupServersBySubscription（单一分组真值，includeEmptyMesh=false —— 其 JSDoc 明确标注
 * 「节点选择器等消费方默认不显空组」，本组件正是该消费方）。直连哨兵见 domain/direct-selection.ts
 * （DIRECT_SERVER_ID，Rust config-engine 已支持，仅前端从未接线——本组件是首个消费方）。
 *
 * 国旗：**按节点名派生**（`flagCodeForName` → `getCountryCode`，74 区域），与节点列表 `NodeCard` 同一口径。
 *
 * 🔴 此处原注释写着「flag 系统本身是全局已知缺口」——**那句话在写下的那一刻就已不成立**：完整旗面系统
 * （`domain/flag-detect` + `flag-assets`，74 区域）与该注释落在同一个 commit，并行分域施工互不知情。
 * 别再信这类注释，信代码。
 *
 * **为什么这里用名称派生、而状态栏/出口节点框不许用**：本选单里的旗是**浏览时的标签**，回答「这个节点
 * 自称在哪」——名称派生在该语境下没有说谎。而状态栏/出口框回答的是「我现在从哪出去」，入口域名可能只是
 * 前置中转，用它冒充出口比不画旗更糟（见 `domain/exit-flag.ts`）。两种语义共用一个符号但数据源必须分开。
 * 识别不到 → 不画（不回退地球：原型的 globe 兜底会让「没识别出来」看起来像「在某个未知国家」）。
 */

import { Fragment, useEffect, useMemo, useState, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import type { ServerConfig, SubscriptionConfig } from '@/contracts/types';
import { groupServersBySubscription, defaultOpenGroupIds } from '@/domain/server-grouping';
import { isBlockSelection, isDirectSelection } from '@/domain/direct-selection';
import { sortServersByLatency } from '@/domain/server-latency-sort';
import { useNodeSortStore } from '@/store/use-node-sort-store';
import { flagCodeForName } from '../nodes/nd-flag';
import { FlagImg } from '@/components/flag-img';
import { latLevel, latDotClass } from '../shared/format';
import { cn } from '@/lib/utils';
import { revealSiblingGroup, useRevealAfterCommit } from '@/components/reveal';

/** 原型两枚 checkmark（同路径、不同 class，分属 .mi 与 .nm-item 两套选择器，见 CK/MK_CK L3016/4552）。 */
const NmCheck = () => (
  <svg className="nm-ck" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
    <path d="M5 12l5 5 9-11" />
  </svg>
);
const MiCheck = () => (
  <svg className="mk-ck" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
    <path d="M5 12l5 5 9-11" />
  </svg>
);

export interface NodeMenuProps {
  open: boolean;
  servers: ServerConfig[];
  subscriptions: SubscriptionConfig[];
  selectedServerId: string | null;
  /** 测速结果：null=超时，undefined=未测。 */
  latencies: Record<string, number | null>;
  onPick: (id: string) => void;
  onPickDirect: () => void;
  onPickBlock: () => void;
  /** 非 null = 「阻断」项禁用并以此为 tooltip 原因（直连模式下阻断不生效）。 */
  blockDisabledReason: string | null;
  onTestAll: (ids: string[]) => void;
  onManage: () => void;
}

export function NodeMenu({
  open,
  servers,
  subscriptions,
  selectedServerId,
  latencies,
  onPick,
  onPickDirect,
  onPickBlock,
  blockDisabledReason,
  onTestAll,
  onManage,
}: NodeMenuProps) {
  const { t } = useTranslation();
  const [search, setSearch] = useState('');
  // 按延迟排序 = **持久化的展示偏好**，不是本地 UI 态：原型 :4475 明写 "single source of truth: every
  // latency-sort switch (toolbar + Home dropdown) reflects st.latencySort; persisted + tray-synced"。
  // 此前这里自造 useState → 与 Nodes 工具栏各记各的、不持久、不同步托盘（契约 L23）。
  const latencySort = useNodeSortStore((s) => s.sortByLatency);
  const toggleLatencySort = useNodeSortStore((s) => s.toggleSortByLatency);
  const [openGroups, setOpenGroups] = useState<Set<string>>(new Set());

  const directOn = isDirectSelection(selectedServerId);
  const blockOn = isBlockSelection(selectedServerId);

  // 对齐原型 toggleNodeMenu：每次打开复位搜索框（renderNodeMenu('')）。
  useEffect(() => {
    if (open) setSearch('');
  }, [open]);

  const groups = useMemo(
    () => groupServersBySubscription(servers, subscriptions, false),
    [servers, subscriptions]
  );

  const q = search.trim().toLowerCase();
  const searching = !!q;
  const filteredGroups = useMemo(() => {
    if (!searching) return groups;
    return groups
      .map((g) => ({
        ...g,
        servers: g.servers.filter(
          (s) =>
            s.name.toLowerCase().includes(q) ||
            s.address.toLowerCase().includes(q) ||
            s.protocol.toLowerCase().includes(q)
        ),
      }))
      .filter((g) => g.servers.length > 0);
  }, [groups, searching, q]);

  const totalCount = groups.reduce((n, g) => n + g.servers.length, 0);
  const listCount = filteredGroups.reduce((n, g) => n + g.servers.length, 0);

  /**
   * 默认展开含当前出口的组，其余折叠；打开菜单时复位。
   *
   * 判据委托 `domain/server-grouping.defaultOpenGroupIds` —— 与托盘「全部节点」、规则弹窗
   * 「目标出站」、应用分流策略菜单**同一条线**（commit 66746e4 把那三处收口到它，本处是漏网的
   * 第四处：它还留着自己那份带 `groups[0]` 回落的局部实现）。
   *
   * 去掉 `groups[0]` 回落是**行为修正**不是等价重构：回落让「默认折叠」恰恰在最需要它的场景
   * 不成立 —— 还没选节点（或选的是直连/阻断哨兵，它不属于任何组）、正要从一堆订阅里挑的时候，
   * 一打开就有一组铺开，而铺开的那组与用户要找的没有任何关系。没有选中项时正确答案是「全折叠」，
   * 不是「猜一个」（66746e4 的原话）。搜索态下 `isOpen` 另有 `searching ||` 腿强制展开，不受影响。
   */
  const defaultOpen = useMemo(
    () => defaultOpenGroupIds(groups, selectedServerId),
    [groups, selectedServerId],
  );

  useEffect(() => {
    if (open) setOpenGroups(new Set(defaultOpen));
  }, [open, defaultOpen]);

  const groupLabel = (g: { isManual: boolean; isMesh?: boolean; name: string }) =>
    g.isManual ? t('nodes.tab.manual') : g.isMesh ? t('nodes.tab.mesh') : g.name;

  const scheduleReveal = useRevealAfterCommit();
  const toggleGroup = (id: string) => {
    setOpenGroups((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const onSwtKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      toggleLatencySort();
    }
  };

  const latText = (v: number | null | undefined): string => {
    if (v === null) return t('home.timeoutShort');
    if (v === undefined) return '';
    return `${v} ms`;
  };

  if (!open) {
    return <div className="node-menu" id="node-menu" role="listbox" aria-label={t('home.nodesMenu')} hidden />;
  }

  return (
    <div className="node-menu" id="node-menu" role="listbox" aria-label={t('home.nodesMenu')}>
      <div className="nm-search" hidden={totalCount < 8}>
        <input
          className="input"
          id="nm-search-inp"
          type="search"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t('home.searchNodesPlaceholder')}
          aria-label={t('home.searchNodesPlaceholder')}
        />
        <label className="nm-sort" data-tip={t('home.sortByLatency')}>
          <span>{t('home.latencyShort')}</span>
          <span
            className={cn('swt', latencySort && 'on')}
            role="switch"
            aria-checked={latencySort}
            tabIndex={0}
            onClick={toggleLatencySort}
            onKeyDown={onSwtKeyDown}
          />
        </label>
      </div>

      {/* `on` 类：选中态的视觉（flow-weak 填充 + flow-hi 文字）与节点行 `.nm-item.on` 同一套
          —— 此前这两行选中时**只多一个勾**，与下面节点行的选中态各说一套（styles/index.css
          「三处节点选择器的视觉统一」段落是那套取值的唯一落点）。 */}
      <button type="button" className={cn('mi', directOn && 'on')} onClick={onPickDirect}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M4 12h16" />
        </svg>
        <span>{t('home.routingDirect')}</span>
        {directOn && <MiCheck />}
      </button>
      {/* 阻断出口紧随直连：两者都是「非节点出口」，图标沿用应用分流「阻断」那条斜杠（AppPolicyScreen
          QUICK_PICKS 的 M5 5l14 14）以统一图形语汇，danger 变体同理。直连模式下禁用 + tooltip 说明
          原因，不留静默 no-op（那时 route.final 恒 = direct，压根没有流量经过 proxy-selector）。

          `act-block-txt` = 动作标签轴的**常驻**红（idle 就显示危险度，不等 hover —— 要 hover 才知道
          点下去会断网是缺陷）。它与 `danger` 并存而非取代：`danger` 只管 hover 那层增量反馈，且它是
          通用破坏性词汇（托盘的「退出」也戴着它），射程不等于本轴。见 styles/index.css「阻断配色两轴」段。 */}
      <button
        type="button"
        className={cn('mi', 'danger', 'act-block-txt', blockOn && 'on', blockDisabledReason && 'disabled')}
        onClick={blockDisabledReason ? undefined : onPickBlock}
        disabled={!!blockDisabledReason}
        data-tip={blockDisabledReason ?? undefined}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M5 5l14 14" />
        </svg>
        <span>{t('home.routingBlock')}</span>
        {blockOn && <MiCheck />}
      </button>
      <div className="mm-sep" />

      {filteredGroups.map((g) => {
        const isOpen = searching || openGroups.has(g.id);
        let items = g.servers;
        // 委托 domain 的单一比较器（与 Nodes 工具栏同口径：无测速结果恒沉底，不随方向翻转）。
        if (latencySort) items = sortServersByLatency(items, (id) => latencies[id]);
        return (
          <Fragment key={g.id}>
            <button
              type="button"
              className="ns-grp"
              aria-expanded={isOpen}
              onClick={(e) => {
                const header = e.currentTarget;
                toggleGroup(g.id);
                scheduleReveal(isOpen ? null : () => revealSiblingGroup(header));
              }}
            >
              <svg
                className={cn('ns-chev', isOpen && 'open')}
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth={2}
              >
                <path d="M9 6l6 6-6 6" />
              </svg>
              <span>{groupLabel(g)}</span>
              <span className="ns-c">{items.length}</span>
            </button>
            {/* 折叠的分组**整棵子树不挂载**。此前是无条件 map + 每行 `hidden`：`hidden` 由
                `.nm-item[hidden]{display:none}` 生效 ⇒ 确实无 layout/paint，但 DOM 节点与 React fiber
                照建不误（实测约 0.3–0.5 MB/100 节点的峰值），而默认展开的只有含当前出口的那一组，
                其余全是用户看不见也点不到的节点行。
                检索真值不受影响：`isOpen` 上面那条 `searching ||` 强制搜索态全组展开，
                「全部测速」读的是 `filteredGroups` 数据而不是 DOM（见 `.nm-foot`）。 */}
            {isOpen && items.map((s) => {
              const lat = latencies[s.id];
              const on = !directOn && s.id === selectedServerId;
              const dead = lat === null;
              return (
                <button
                  key={s.id}
                  type="button"
                  className={cn('nm-item', on && 'on', dead && 'dead')}
                  role="option"
                  aria-selected={on}
                  onClick={() => onPick(s.id)}
                >
                  <span className={cn('nm-latdot', latDotClass(lat))} />
                  {/* 名称派生（语义 =「这个节点自称在哪」，见文件头注）。渲染器与状态栏/出口框共用
                      `FlagImg`——需要分开的是**数据源**，不是渲染体。默认 `.flag` 盒（18×12）即行内尺寸。 */}
                  <FlagImg code={flagCodeForName(s.name)} />
                  <span className="nm-name">{s.name}</span>
                  <span className="nm-badge">{s.protocol.toUpperCase()}</span>
                  <span className={cn('nm-lat', latLevel(lat))}>{latText(lat)}</span>
                  {on && <NmCheck />}
                </button>
              );
            })}
          </Fragment>
        );
      })}

      {listCount === 0 && <div className="nm-empty">{t('nodes.emptyFiltered')}</div>}

      <div className="nm-foot">
        <button
          type="button"
          className="btn ghost sm"
          onClick={() => onTestAll(filteredGroups.flatMap((g) => g.servers.map((s) => s.id)))}
        >
          <span>{t('nodes.testAll')}</span>
        </button>
        <button type="button" className="btn ghost sm" onClick={onManage}>
          <span>{t('home.manageNodesArrow')}</span>
        </button>
      </div>
    </div>
  );
}

export default NodeMenu;
