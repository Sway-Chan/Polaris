/**
 * SubInfoBar —— 订阅信息栏（1:1 提取自原型 polaris-prototype.html L1762-1779 .sub-info.sub-info2）。
 *
 * 原型 DOM（.sub-info.sub-info2，样式见 src/styles/screens.css L「subscription info bar」段）：
 *   .si-main（订阅名 + 数量 pill + 自动/手动标签 + 操作按钮：刷新/编辑/删除/更多）
 *   .si-meta（URL + 更新时间 + 流量用量条 + 到期）
 *
 * 数据源：SubscriptionConfig（store.config.subscriptions）+ userInfo（流量/到期）。
 *
 * 「更多」按钮（原型 data-act="sub-menu" :1770 / `subMenu()` :4778）：**已接线**，弹五项迷你菜单。
 * 各项落点（全部真链路，无占位）：
 *   - 重命名 / 编辑 URL：开 SubDialog（`api.subscription.update` 原地改 name+url），分别 autoFocus
 *     到名称 / URL 输入框——两项落到同一弹窗的不同字段，不是两个同义按钮；
 *   - 复制 URL：`navigator.clipboard`（纯前端，无需后端）；
 *   - 更新间隔…：Polaris **无 per-sub 间隔字段**（`SubscriptionConfig` 无该键，调度器亦只读全局
 *     `subscriptionUpdateIntervalHours`）→ 不伪造 per-sub 设置，改为跳转设置→更新，标签明写「全局」；
 *   - 删除订阅：带连带移除节点数 + 二次确认（删订阅会连删其下全部节点，不可撤销）。
 *
 * 自动更新徽标：状态由 `nodes-logic.ts::subAutoUpdateStatus` 三态判定（per-sub 开关 × 全局总开关 ×
 * 间隔「仅手动」）。**订正早前的陈旧注释**——彼时注释称「全仓没有任何调度器消费 subscription.autoUpdate」，
 * 现已不实：`runtime/subscription_scheduler.rs::select_due` :148 逐条读 per-sub `autoUpdate`。
 * 绿点只在三门全通（真会按周期刷新）时给；其余给中性点 + 说明是哪一道门拦住的，间隔数字取真实配置
 * （原先写死 "12h"，用户改成 24h 后徽标仍显 12h）。
 */

import { useTranslation } from 'react-i18next';
import type { SubscriptionConfig } from '@/contracts/types';
import type { SubscriptionUpdateProgress } from '@/contracts/subscription-progress';
import { fmtBytes } from '@/components/screens/shared/format';
import { cn } from '@/lib/utils';
import { relativeTimeTextIso } from '@/lib/relative-time';
import { useEffect, useRef, useState } from 'react';
import { useAnchoredMenu } from '@/lib/use-anchored-menu';
import {
  subAutoUpdateStatus,
  subEffectiveIntervalHours,
  subUsage,
  type SubAutoUpdateConfigLike,
  type SubMenuItem,
} from './nodes-logic';

export interface SubInfoBarProps {
  subscription: SubscriptionConfig;
  /** 该订阅下节点数。 */
  nodeCount: number;
  /** 自动更新徽标的三态判定输入（全局总开关 + 全局间隔）。 */
  config?: SubAutoUpdateConfigLike | null;
  onRefresh?: (sub: SubscriptionConfig) => void;
  onEdit?: (sub: SubscriptionConfig) => void;
  onDelete?: (sub: SubscriptionConfig) => void;
  /**
   * 「更多」菜单项点击（原型 sub-menu :4778 五项）。菜单本体由本组件渲染（锚定 = 更多按钮），
   * 动作交给调用方——弹窗/剪贴板/导航/确认框都是屏级能力，信息栏不该自己持有。
   * 未传 → 按钮如实 disabled + data-tip 说明（B5：不留惰性占位）。
   */
  onMenuAction?: (item: SubMenuItem, sub: SubscriptionConfig) => void;
  /** 删除项文案里的连带移除节点数（原型逐字带上「移除 48 个节点」）。 */
  deleteNodeCount?: number;
  /**
   * 本订阅的更新进度（`store/use-subscription-progress-store`）。`null`/缺省 = 无事发生。
   *
   * 手动刷新与**后台 scheduler** 共用同一后端发射点 ⇒ 这里的状态不区分谁发起的，自动刷新期间
   * 同样会亮（这正是原先完全没有的那一半：用户不在场时节点数悄悄变了，界面零交代）。
   */
  progress?: SubscriptionUpdateProgress | null;
}

function RefreshIcon() {
  return (
    <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M4 4v6h6M20 20v-6h-6" />
      <path d="M4 10a8 8 0 0114-3M20 14a8 8 0 01-14 3" />
    </svg>
  );
}
function EditIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M12 20h9M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z" />
    </svg>
  );
}
function DeleteIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13" />
    </svg>
  );
}
function ClockIcon() {
  return (
    <svg viewBox="0 0 24 24" width={12} fill="none" stroke="currentColor" strokeWidth={1.8}>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7v5l3 2" />
    </svg>
  );
}
function MoreIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <circle cx="5" cy="12" r="1.6" />
      <circle cx="12" cy="12" r="1.6" />
      <circle cx="19" cy="12" r="1.6" />
    </svg>
  );
}

/** 「更多」菜单里的图标（原型 subMenu 逐项 svgI 路径逐字对齐）。 */
function MenuIcon({ d }: { d: string }) {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d={d} />
    </svg>
  );
}

export function SubInfoBar({
  subscription,
  nodeCount,
  config,
  onRefresh,
  onEdit,
  onDelete,
  onMenuAction,
  deleteNodeCount,
  progress,
}: SubInfoBarProps) {
  const { t } = useTranslation();

  // 「更多」迷你菜单（开合 + 外部点击/ESC 关闭，同 NodesScreen 既有 addMenu/tsMenu 写法）。
  const [menuOpen, setMenuOpen] = useState(false);
  const menuWrapRef = useRef<HTMLDivElement>(null);
  /* 定位 + 首项聚焦收口到 `useAnchoredMenu`（原型 miniMenu :3245-3253）；此前是零 clamp 的 CSS 锚定。 */
  const anchored = useAnchoredMenu<HTMLButtonElement, HTMLDivElement>(menuOpen && !!onMenuAction, 'right');
  useEffect(() => {
    if (!menuOpen) return;
    const onDown = (e: MouseEvent) => {
      if (!menuWrapRef.current?.contains(e.target as Node)) setMenuOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenuOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [menuOpen]);
  // 订阅切换（tab 切走）时菜单不该残留在新订阅上。
  useEffect(() => setMenuOpen(false), [subscription.id]);

  const runMenu = (item: SubMenuItem) => {
    setMenuOpen(false);
    onMenuAction?.(item, subscription);
  };

  const autoStatus = subAutoUpdateStatus(subscription, config);
  const intervalHours = subEffectiveIntervalHours(config);

  // 进度分两处落地，刻意不合并：
  //  · **进行中** → 顶掉 si-main 那颗自动更新徽标。它是瞬态的，且更新正在进行时「下次几点自动刷」
  //    恰恰是此刻最不相关的一条信息；
  //  · **失败** → 落在 si-meta 的「更新时间」旁边（语义就是「上次更新的结局」），**不**顶掉自动徽标
  //    —— 失败是会长期挂着的状态，永久遮住另一条真信息不可接受。
  const updating = progress !== null && progress !== undefined && progress.phase !== 'failed';
  const failure = progress?.phase === 'failed' ? progress : null;

  /** 进行中阶段 → 一行短文案（终态不走这里：done/unchanged 在 store 里就被删了，failed 另有分支）。 */
  const phaseLabel = () => {
    switch (progress?.phase) {
      case 'providers':
        // 全库唯一有真计数的阶段（provider 串行子拉取，每个最长 15s）。
        return t('nodes.subUpdatingProviders', {
          done: progress.done ?? 0,
          total: progress.total ?? 0,
        });
      case 'applying':
        return t('nodes.subUpdatingApplying');
      default:
        return t('nodes.subUpdatingFetching');
    }
  };

  const ui = subscription.userInfo;
  // 阈值/百分比派生归 nodes-logic（契约数字须可单测；此前内联写 80，比契约的 85 早一档变红）。
  const { used: usedBytes, total: totalBytes, pct, warn: usageWarn } = subUsage(ui);
  const updatedText = relativeTimeTextIso(subscription.lastUpdated, t);
  const expiryText = ui?.expire
    ? new Date(ui.expire * 1000).toISOString().slice(0, 10)
    : null;

  return (
    <div className="sub-info sub-info2">
      <div className="si-main">
        <b>{subscription.name}</b>
        <span className="pill region">{nodeCount}</span>
        {/* 更新进行中 → 顶掉自动更新徽标（见上方 updating/failure 的分处理由）。 */}
        {updating ? (
          <span
            className="pill warn si-auto"
            role="status"
            data-tip={t('nodes.subUpdatingHint')}
          >
            <span className="spinner spin-inline" />
            {phaseLabel()}
          </span>
        ) : /* 自动更新徽标三态（判定见 nodes-logic.subAutoUpdateStatus）：
            绿点 = 调度器真会按周期刷新；中性点 = 开关虽开但被上游门拦住，title 说明是哪一道。 */
        autoStatus === 'active' ? (
          <span
            className="pill ok si-auto"
            data-tip={t('nodes.subAutoUpdateActiveHint', {
              h: intervalHours,
            })}
          >
            <RefreshIcon />
            {t('nodes.subAutoUpdateEvery', { h: intervalHours })}
          </span>
        ) : autoStatus === 'master-off' ? (
          <span
            className="pill region si-auto"
            data-tip={t('nodes.subAutoUpdateMasterOffHint')}
          >
            <RefreshIcon />
            {t('nodes.subAutoUpdatePaused')}
          </span>
        ) : autoStatus === 'startup-only' ? (
          <span
            className="pill region si-auto"
            data-tip={t('nodes.subAutoUpdateStartupOnlyHint')}
          >
            <RefreshIcon />
            {t('nodes.subAutoUpdateStartupOnly')}
          </span>
        ) : (
          <span className="pill region si-auto">
            <ClockIcon />
            {t('nodes.subManualUpdate')}
          </span>
        )}
        <div className="si-acts">
          {/* 更新期间禁点：后端没有单飞闸，连点会真的并发拉两次同一订阅（两次对账互相覆盖）。 */}
          <button
            type="button"
            className="btn ghost sm"
            disabled={updating}
            onClick={() => onRefresh?.(subscription)}
          >
            {updating ? <span className="spinner spin-inline" /> : <RefreshIcon />}
            <span>{updating ? t('nodes.subUpdating') : t('nodes.subRefresh')}</span>
          </button>
          <button
            type="button"
            className="nd-a"
            onClick={() => onEdit?.(subscription)}
            data-tip={t('nodes.subEditTitle')}
            aria-label={t('nodes.subEditTitle')}
          >
            <EditIcon />
          </button>
          <button
            type="button"
            className="nd-a err"
            onClick={() => onDelete?.(subscription)}
            data-tip={t('nodes.subDeleteTitle')}
            aria-label={t('nodes.subDeleteTitle')}
          >
            <DeleteIcon />
          </button>
          <div ref={menuWrapRef} style={{ position: 'relative', display: 'inline-flex' }}>
            <button
              ref={anchored.anchorRef}
              type="button"
              className="nd-a"
              id="sub-more"
              aria-haspopup="menu"
              aria-expanded={menuOpen}
              onClick={() => setMenuOpen((v) => !v)}
              disabled={!onMenuAction}
              style={!onMenuAction ? { opacity: 0.5, pointerEvents: 'none' } : undefined}
              data-tip={
                onMenuAction
                  ? t('nodes.subMenu')
                  : t('nodes.subMenuDisabledHint')
              }
              aria-label={t('nodes.subMenu')}
            >
              <MoreIcon />
            </button>
            {menuOpen && onMenuAction && (
              <div ref={anchored.menuRef} className="mini-menu" role="menu" style={anchored.style}>
                <div className="mm-lbl">{t('nodes.subMenuHeader')}</div>
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  data-act="sub-rename"
                  onClick={() => runMenu('rename')}
                >
                  <MenuIcon d="M12 20h9M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z" />
                  <span>{t('nodes.subRename')}</span>
                </button>
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  data-act="sub-edit-url"
                  onClick={() => runMenu('edit-url')}
                >
                  <MenuIcon d="M9 15l6-6M8 8a3 3 0 10-3 3" />
                  <span>{t('nodes.subEditUrl')}</span>
                </button>
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  data-act="sub-copy-url"
                  onClick={() => runMenu('copy-url')}
                >
                  <MenuIcon d="M9 15l6-6M8 8a3 3 0 10-3 3M16 16a3 3 0 103 3" />
                  <span>{t('nodes.subCopyUrl')}</span>
                </button>
                {/* Polaris 无 per-sub 间隔字段（调度器只读全局）→ 不做假的"本订阅间隔"，
                    如实标「全局」并跳到设置里那唯一一处真开关。 */}
                <button
                  type="button"
                  className="mi"
                  role="menuitem"
                  data-act="sub-interval"
                  onClick={() => runMenu('interval')}
                  data-tip={t('nodes.subIntervalHint')}
                >
                  <MenuIcon d="M12 8v5M12 16h.01" />
                  <span>{t('nodes.subInterval')}</span>
                </button>
                <div className="mm-sep" />
                <button
                  type="button"
                  className="mi danger"
                  role="menuitem"
                  data-act="sub-del-menu"
                  onClick={() => runMenu('delete')}
                >
                  <MenuIcon d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13" />
                  <span>
                    {t('nodes.subDeleteWithCount', {
                      count: deleteNodeCount ?? nodeCount,
                    })}
                  </span>
                </button>
              </div>
            )}
          </div>
        </div>
      </div>
      <div className="si-meta">
        <span className="si-url mono">{subscription.url}</span>
        {updatedText && (
          <span className="si-updated">
            <ClockIcon />
            <span>{updatedText}</span>
          </span>
        )}
        {/* 失败徽标**长期挂着**，直到下一次更新成功（store 侧 done/unchanged 删键）。
            理由：toast 2.2s 就散，而「这条订阅现在是停更的」是持续为真的状态；后台自动更新失败时
            用户根本不在场，toast 白弹。tooltip 逐字给后端真错因（不用笼统兜底文案顶替真值）。 */}
        {failure && (
          <span
            className="pill err"
            role="status"
            data-tip={failure.error || t('nodes.subRefreshFail')}
          >
            {t('nodes.subUpdateFailed')}
          </span>
        )}
        {totalBytes > 0 && (
          <span className={cn('sub-usage', usageWarn && 'warn')}>
            {/* fmtBytes 自带单位（B/KB/MB/GB/TB）——此前额外拼了个 " GB"，1.2TB 会渲染成
                「1.20 TB GB」。单位只能有一处来源。 */}
            <span>
              {fmtBytes(usedBytes)} / {fmtBytes(totalBytes)}
            </span>
            <span className="bar">
              <i style={{ width: `${pct}%` }} />
            </span>
          </span>
        )}
        {expiryText && <span className="si-expiry">{t('nodes.subExpiry', { date: expiryText })}</span>}
      </div>
    </div>
  );
}

export default SubInfoBar;
