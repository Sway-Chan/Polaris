/**
 * RuleItem —— 单条路由规则行（1:1 提取自原型 polaris-prototype.html L1911-1915 .rule-item）。
 *
 * 原型 DOM（.rule-item，样式见 src/styles/prototype.css「RULES」段，勿改该文件）：
 *   .rule-pri（优先级序号）+ .rule-grip（拖拽手柄）+ .rule-type-ic（类型图标）
 *   + .rule-main（b 标题 + .rmeta 条件摘要，hover 详情卡=原型 data-tipcard="rule"）
 *   + .rule-target（.pill act-*（阻断/直连/代理，key 与 RuleHoverCard 共用）+ .rt-node 目标节点，margin-top:3px）
 *   + .rule-acts（置顶/上移/下移/置底 + .swt 启停开关 + 复制 + 编辑 + 删除，后两颗见原型
 *     `enhanceRuleRow` :4782 注入的 rule-copy / rule-del；文案走 i18n：rules.toggleEnabled /
 *     rules.duplicate / common.edit / common.delete）
 *
 * 数据源：Rule（store.rules）。条件摘要用 domain/rules.ruleConditions 汇总。
 */

import { useTranslation } from 'react-i18next';
import type { Rule, RuleCondition } from '@/contracts/types';
import { ruleConditions } from '@/domain/rules';
import { RULE_TYPE_CATEGORY } from '@/domain/rules';
import { cn } from '@/lib/utils';
import { useHoverCard, HoverCardPanel } from '@/components/hover-cards/HoverCard';
import { RuleHoverCardContent, TYPE_LABEL } from '@/components/hover-cards/RuleHoverCard';

/**
 * 单枚条件计数标签：`域名后缀 ×15`，悬停出该类型的全部值。
 *
 * 拆成组件而非在 [`RuleMetaCounts`] 里循环渲染：`useHoverCard` 是 hook，不能在 map 里调 ——
 * 每枚标签要有各自独立的开合状态与定位，只能一枚一个组件实例。
 */
function CondCount({ cond }: { cond: RuleCondition }) {
  const { t } = useTranslation();
  const hc = useHoverCard<HTMLSpanElement>();
  const label = t(`rules.types.${cond.type}.name`, TYPE_LABEL[cond.type] ?? cond.type);
  const n = cond.values.length;
  return (
    <span className="rmeta-cnt" ref={hc.triggerRef} {...hc.triggerHandlers}>
      {label}
      <span className="rmeta-n">×{n}</span>
      <HoverCardPanel
        cardRef={hc.cardRef}
        open={hc.open}
        pos={hc.pos}
        onMouseEnter={hc.cardHandlers.onMouseEnter}
        onMouseLeave={hc.cardHandlers.onMouseLeave}
      >
        <div className="tip-t">
          {label} · {n}
        </div>
        <div className="rmeta-vals mono">{cond.values.join(', ')}</div>
      </HoverCardPanel>
    </span>
  );
}

/**
 * 条件摘要 —— **每个条件类型只出一枚「类型 ×n」计数标签，值明细走标签自身的 hover**。
 *
 * # 为什么不逐值平铺（原实现）
 *
 * 原实现把每个条件的全部值逗号拼成一整串塞进 `.rmeta`，而该类无行数限制 ⇒ 值一多就把规则行撑成
 * 好几行。真机实测（2026-08-03）「券商」一条 = `域名后缀 ×15 · 域名关键词 ×2 · 规则集 ×4` 共 21 个值，
 * 一行铺开占掉整屏可视规则的一大半。
 *
 * **一律计数、不做「少量显值」的分档**（陈先生 2026-08-03 定）：分档会让行高随规则内容跳动，观感不统一；
 * 且值的条数只会随时间膨胀，今天「只有 1 个值」的规则明天就不是了 —— 固定形态才防得住后续膨胀。
 *
 * 连接符沿用原语义：`combineMode==='and'` 用 `∧`（全部满足），否则 `·`（满足任一）。
 */
export function RuleMetaCounts({ rule }: { rule: Rule }) {
  const conds = ruleConditions(rule);
  const sep = rule.combineMode === 'and' ? '∧' : '·';
  return (
    <div className="rmeta">
      {conds.map((c, i) => (
        <span key={i}>
          {i > 0 && <span className="rmeta-sep">{sep}</span>}
          <CondCount cond={c} />
        </span>
      ))}
    </div>
  );
}

/** 规则标题：remarks 优先，否则用首条件类型大写。 */
function ruleTitle(rule: Rule): string {
  if (rule.remarks && rule.remarks.trim()) return rule.remarks;
  return rule.type;
}

/** 规则类型分类图标（domain/network/device/process/ruleset）。 */
function TypeIcon({ rule }: { rule: Rule }) {
  const cat = RULE_TYPE_CATEGORY[rule.type];
  // ruleset（geosite/geoip/ruleSet）→ globe；process → 窗口；network → 端口线；device → 网卡；domain → 地球带线
  switch (cat) {
    case 'ruleset':
      return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <circle cx="12" cy="12" r="9" />
          <path d="M3 12h18M12 3c3 3 3 15 0 18" />
        </svg>
      );
    case 'process':
      return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <rect x="4" y="4" width="16" height="16" rx="2" />
          <path d="M9 9h6v6" />
        </svg>
      );
    case 'network':
      return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M4 7h16M4 12h16M4 17h10" />
        </svg>
      );
    case 'device':
      return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <rect x="3" y="4" width="18" height="12" rx="1.5" />
          <path d="M8 20h8M12 16v4" />
        </svg>
      );
    default: // domain
      return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <circle cx="12" cy="12" r="9" />
          <path d="M12 2v20M2 12h20" />
        </svg>
      );
  }
}

/** 动作 → pill 类 + 文案。block = 阻断，direct = 直连，proxy = 代理。key 与 RuleHoverCard 共用，避免同行 hover 卡文案漂移。 */
function actionPill(action: Rule['action'], t: (key: string, defaultValue: string) => string): { cls: string; text: string } {
  switch (action) {
    case 'block':
      return { cls: 'pill act-block', text: t('rules.targetBlock', '阻断') };
    case 'direct':
      return { cls: 'pill act-direct', text: t('rules.targetDirect', '直连') };
    default:
      return { cls: 'pill act-proxy', text: t('rules.policyProxy', '代理') };
  }
}

function GripIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <circle cx="9" cy="6" r="1" />
      <circle cx="15" cy="6" r="1" />
      <circle cx="9" cy="12" r="1" />
      <circle cx="15" cy="12" r="1" />
      <circle cx="9" cy="18" r="1" />
      <circle cx="15" cy="18" r="1" />
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

export interface RuleItemProps {
  rule: Rule;
  /** 1-based 优先级序号。 */
  index: number;
  /** 目标节点显示名（action=proxy 时，由调用方从 targetServerId 解析）。 */
  targetNodeName?: string;
  /**
   * 角标①：本规则引用的规则资源本地缺失（已删/文件坏）。判据 `domain/rule-resource-refs.missingResourceRuleIds`
   * （**fail-closed**：生成配置时该条件会被整条跳过 → 规则静默失效）。
   */
  hasMissingResource?: boolean;
  /** 角标②：本规则的 ipCidr 与组网 force-route 段重叠（判据 `domain/mesh-rule-overlap.meshOverlapRuleIds`）。 */
  hasMeshOverlap?: boolean;
  /**
   * 角标③：`action==='proxy'` 且指定了 `targetServerId`，但该节点已被删除 → 运行时回退为跟随全局。
   * 由调用方判定（它持有 servers 表）：`!!rule.targetServerId && !serverNameById.has(...)`。
   */
  targetMissing?: boolean;
  /**
   * 角标④：该规则只存在于**暂存**里、磁盘上还没有（判据 `lib/staged-config.stagedOnlyIds` =
   * effective − disk）。列表读的是 effective（否则用户看不见刚做的编辑），标记让「它还没落盘」
   * 这件事在列表里可见。用词与 pending-bar 的「N 项待保存」同源，不另起一套。
   */
  stagedOnly?: boolean;
  onToggle?: (rule: Rule) => void;
  onEdit?: (rule: Rule) => void;
  /**
   * 行内复制（G5，原型 `enhanceRuleRow` :4771-4773 的 `rule-copy`）。
   *
   * 未传 = 不渲染按钮（本组件另有只读消费点，别给它们塞一个点了没反应的按钮）。
   */
  onDuplicate?: (rule: Rule) => void;
  /**
   * 行内删除（原型 `enhanceRuleRow` :4782 注入的 `rule-del` 垃圾桶 + :4097 的 `confirmTwice`）。
   *
   * 未传 = 不渲染按钮（同 `onDuplicate`：本组件另有只读消费点，别给它们塞一个点了没反应的按钮）。
   * 删除本体（暂存分流 / 撤销条目 / 直落盘）在 `lib/use-rule-delete.ts`，与规则弹窗 footer 那颗共用。
   */
  onDelete?: (rule: Rule) => void;
  /**
   * 本行的删除按钮处于「再点一次即删」待定态。状态由 `RulesScreen` 的 `useConfirmTwice` 单点持有
   * （同 NodeCard：行不自持，否则每行一份定时器，又变回「各写各的」）。
   */
  deleteConfirming?: boolean;
  /** 拖拽：开始拖本行 / 落到本行上 / 拖入新位置。 */
  onDragStart?: (rule: Rule) => void;
  onDragOver?: (rule: Rule, e: React.DragEvent) => void;
  onDrop?: (rule: Rule) => void;
  isDragging?: boolean;
  /** 上下移 / 置顶底（undefined = 该方向已到边界，按钮 disabled）。 */
  onMove?: (rule: Rule, to: 'up' | 'down' | 'top' | 'bottom') => void;
  /** 本行是否为列表首行 / 末行（决定上移·置顶 / 下移·置底 的 disabled）。 */
  isFirst?: boolean;
  isLast?: boolean;
}

export function RuleItem({
  rule,
  index,
  targetNodeName,
  hasMissingResource,
  hasMeshOverlap,
  targetMissing,
  stagedOnly,
  onToggle,
  onEdit,
  onDuplicate,
  onDelete,
  deleteConfirming,
  onDragStart,
  onDragOver,
  onDrop,
  isDragging,
  onMove,
  isFirst,
  isLast,
}: RuleItemProps) {
  const { t } = useTranslation();
  const act = actionPill(rule.action, t);
  // 规则名 hover 详情卡（设计文档 §1.5 RuleHoverCard；原型 :3964 data-tipcard="rule" 挂在 .rule-main 上）。
  const hc = useHoverCard<HTMLSpanElement>();
  return (
    <div
      className={cn('rule-item', !rule.enabled && 'off', isDragging && 'dragging')}
      draggable
      onDragStart={() => onDragStart?.(rule)}
      onDragOver={(e) => onDragOver?.(rule, e)}
      onDrop={() => onDrop?.(rule)}
    >
      <span className="rule-pri">{index + 1}</span>
      <span className="rule-grip" aria-hidden>
        <GripIcon />
      </span>
      {/* hover 详情卡触发器挂在类型图标（logo）上，非整个 rule-main——用户要求 hover 只在 logo 触发，
          停在标题/节点切换区不弹，避免遮挡操作。 */}
      <span className="rule-type-ic" ref={hc.triggerRef} {...hc.triggerHandlers}>
        <TypeIcon rule={rule} />
      </span>
      <div className="rule-main">
        <b>{ruleTitle(rule)}</b>
        <RuleMetaCounts rule={rule} />
        <HoverCardPanel
          cardRef={hc.cardRef}
          open={hc.open}
          pos={hc.pos}
          onMouseEnter={hc.cardHandlers.onMouseEnter}
          onMouseLeave={hc.cardHandlers.onMouseLeave}
        >
          <RuleHoverCardContent rule={rule} targetNodeName={targetNodeName} />
        </HoverCardPanel>
      </div>
      <div className="rule-target">
        <div style={{ display: 'inline-flex', alignItems: 'center', gap: 5, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
          <span className={act.cls}>{act.text}</span>
          {/* 角标②「覆盖组网」：中性色（pill region）——它不是错误，是「你可能没意识到的优先级后果」，
              与真正失效的①③（warn 色）分级，避免把两种严重度混成一片橙。文案/口径对齐 上游
              sortable-rule-row.tsx:226-236。 */}
          {stagedOnly && (
            <span
              className="pill"
              data-tip={t('home.stagedOnlyHint', '尚未保存到配置文件：内核还看不到它，节点也暂时不能被选为出口。点条上的「保存」后生效。')}
            >
              {t('home.stagedOnlyBadge', '待保存')}
            </span>
          )}
          {hasMeshOverlap && (
            <span className="pill region" data-tip={t('rules.meshOverlapTip', '该规则网段与组网(WG/Tailscale)路由段重叠，优先级更高将覆盖组网路由，该段可能不走组网节点。如非有意请调整。')}>
              {t('rules.meshOverlap', '覆盖组网')}
            </span>
          )}
          {/* 角标①「资源缺失」：fail-closed 语义的唯一 UI 出口。后端生成配置时会静默跳过该条件
              （route.rs 按 fileExists 过滤），不标就等于规则失效而用户无感（上游 :237-247）。 */}
          {hasMissingResource && (
            <span className="pill warn" data-tip={t('rules.resourceMissingTip', '该规则引用的规则资源缺失（已删除或文件丢失），当前不生效；请到「规则资源」页下载恢复')}>
              {t('rules.resourceMissing', '资源缺失')}
            </span>
          )}
        </div>
        {/* 角标③「节点已失效」：指定出口节点被删 → 运行时回退跟随全局（上游 rules-page.tsx:215-224）。
            与 `→ 节点名` 互斥：节点还在就显示名字，删了就显示角标，绝不显示空箭头。 */}
        {rule.action === 'proxy' && targetMissing ? (
          <div style={{ marginTop: 3 }}>
            <span className="pill warn" data-tip={t('rules.targetMissingTip', '指定节点已删除，运行时回退为跟随全局选中节点')}>
              {t('rules.targetMissing', '节点已失效')}
            </span>
          </div>
        ) : (
          rule.action === 'proxy' &&
          targetNodeName && (
            <div className="rt-node" style={{ marginTop: 3 }}>→ {targetNodeName}</div>
          )
        )}
      </div>
      <div className="rule-acts">
        {/* 上下移 / 置顶底：原生拖拽在长列表 + 触控板上极难命中（且屏外目标要靠自动滚动），
            键盘用户更是完全无从排序。与拖拽共用同一条 reorder 提交路径。 */}
        {onMove && (
          <span className="rule-move" style={{ display: 'inline-flex', gap: 2 }}>
            <button
              type="button"
              className="nd-a"
              disabled={isFirst}
              onClick={() => onMove(rule, 'top')}
              data-tip={t('rules.moveTop', '置顶')}
              aria-label={t('rules.moveTop', '置顶')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M5 5h14M12 20V9M7 14l5-5 5 5" />
              </svg>
            </button>
            <button
              type="button"
              className="nd-a"
              disabled={isFirst}
              onClick={() => onMove(rule, 'up')}
              data-tip={t('rules.moveUp', '上移')}
              aria-label={t('rules.moveUp', '上移')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M12 19V6M6 12l6-6 6 6" />
              </svg>
            </button>
            <button
              type="button"
              className="nd-a"
              disabled={isLast}
              onClick={() => onMove(rule, 'down')}
              data-tip={t('rules.moveDown', '下移')}
              aria-label={t('rules.moveDown', '下移')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M12 5v13M6 12l6 6 6-6" />
              </svg>
            </button>
            <button
              type="button"
              className="nd-a"
              disabled={isLast}
              onClick={() => onMove(rule, 'bottom')}
              data-tip={t('rules.moveBottom', '置底')}
              aria-label={t('rules.moveBottom', '置底')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M5 19h14M12 4v11M7 10l5 5 5-5" />
              </svg>
            </button>
          </span>
        )}
        <span
          className={cn('swt', rule.enabled && 'on')}
          role="switch"
          aria-checked={rule.enabled}
          tabIndex={0}
          data-tip={t('rules.toggleEnabled', '启用 / 停用')}
          onClick={() => onToggle?.(rule)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onToggle?.(rule);
            }
          }}
        />
        {onDuplicate && (
          <button
            type="button"
            className="nd-a"
            onClick={() => onDuplicate(rule)}
            data-tip={t('rules.duplicate', '复制')}
            aria-label={t('rules.duplicate', '复制')}
          >
            {/* 双叠矩形 = 通用「复制」形，与连接页右键菜单的复制图标同源 */}
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
              <path d="M9 9h10v10H9zM5 15V5h10" />
            </svg>
          </button>
        )}
        <button
          type="button"
          className="nd-a"
          onClick={() => onEdit?.(rule)}
          data-tip={t('common.edit', '编辑')}
          aria-label={t('common.edit', '编辑')}
        >
          <EditIcon />
        </button>
        {/* 行内删除 = 原地二次点击（原型 :4097 `rule-del`）。原型这颗是 `enhanceRuleRow` :4782 注入的
            图标按钮（`nd-a` + 垃圾桶 svg + `color:hsl(var(--err))`，无 `<span>`）⇒ 原型 confirmTwice
            对无 span 的按钮不换文案，确认态**只**靠 `.confirming` 翻红（components.css:958）。
            本仓照搬这条，另把 data-tip/aria-label 换成「再点一次确认」——纯颜色状态对键盘/读屏用户
            不可达，那是本仓 DOM 的补齐而非对原型的加戏（口径逐字同 NodeCard 那颗）。
            此前删规则的唯一入口是编辑弹窗 footer，列表里删一条要先开窗（陈先生 2026-07-30）。 */}
        {onDelete && (
          <button
            type="button"
            className={cn('nd-a err', deleteConfirming && 'confirming')}
            onClick={() => onDelete(rule)}
            data-tip={deleteConfirming ? t('common.confirmAgain', '再点一次确认') : t('common.delete', '删除')}
            aria-label={
              deleteConfirming ? t('common.confirmAgain', '再点一次确认') : t('common.delete', '删除')
            }
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13" />
            </svg>
          </button>
        )}
      </div>
    </div>
  );
}

export default RuleItem;
