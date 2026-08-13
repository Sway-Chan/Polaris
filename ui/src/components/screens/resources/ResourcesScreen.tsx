/**
 * ResourcesScreen —— 规则资源屏（1:1 提取自原型 polaris-prototype.html L1941-1973 #s-resources）。
 *
 * 原型 DOM（class/层级对齐，样式见 src/styles/screens.css L「RESOURCES」段）：
 *   .screen
 *     .phead（标题 + .sub + .acts：重置内置 / 资源库 / URL 下载）
 *     .card-h「已下载资源」
 *     .seg2 源过滤（全部/内置/外置）+「全部更新」
 *     .res-table > .res-row.head + [.res-grp + .res-row]*（按分类分组，引用徽章）
 *
 * 数据流：api.ruleResources.list()（RuleResourceListItem[]）。下载/更新/重置经同 api。
 * 进度经 api.ruleResources.onProgress。
 *
 * # 内置 geo 行（`item.builtin`）—— 曾整体删除，本批恢复
 *
 * 删除时的判据是「后端 `rule_resources_list` 只列 `config.ruleResources`，`builtin` 恒 undefined ⇒
 * 四处分支全是死代码」，那在当时成立。两条阻塞已消解：后端补了 builtin 条目，行内更新对内置项走
 * `updateBuiltin(tag)` 而非 `redownload(id)`（后者按 id 查册，对 `builtin:*` 恒 `RULE_RESOURCE_NOT_FOUND`）。
 *
 * 内置行与外置行的三处行为差异：
 * - **更新腿不同**（见上）；
 * - **不可删**：内置 geo 被智能分流隐式依赖，后端 `rule_resources_delete` 对其也无路径 ⇒ 入口不渲染；
 * - **引用徽章走 `ref-badge sys`**：它们被智能分流隐式引用，`referencedBy` 实算恒为 0，
 *   显示「未引用」会误导用户去删（而删不掉）。
 */

import { Fragment, useEffect, useMemo, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/ipc';
import type {
  RuleResourceListItem,
  RuleResourceProgress,
  RuleResourceCategory,
} from '@/contracts/types';
import { fmtBytes } from '@/components/screens/shared/format';
import { categoryLabel } from '@/domain/rule-resource-catalog';
import { cn } from '@/lib/utils';
import { toast } from '@/lib/error-handler';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import { useEffectiveConfig, useEffectiveRules } from '@/store/app-store';
import { useAppPresetsStore } from '@/store/use-app-presets-store';
import { useHoverCard, HoverCardPanel } from '@/components/hover-cards/HoverCard';
import { resourceRefs, ResourceRefsHoverCardContent, type ResourceRef } from '@/components/hover-cards/ResourceRefsHoverCard';

/** 资源来源过滤档（原型 .seg2「全部 / 内置 / 外置」）。 */
type SourceFilter = 'all' | 'builtin' | 'external';

/** 相对时间（"2 小时前" / "1 天前"）。 */
function relativeTime(iso: string | undefined): string {
  if (!iso) return '—';
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return '—';
  const diffH = (Date.now() - then) / 3_600_000;
  if (diffH < 1) return '刚刚';
  if (diffH < 24) return `${Math.floor(diffH)} 小时前`;
  if (diffH < 48) return '昨天';
  return `${Math.floor(diffH / 24)} 天前`;
}

/** 「重置内置资源」的原地二次确认 key（原型 :4198 `geo-reset`）。 */
const GEO_RESET_KEY = 'geo-reset';
/** 单条资源删除的 key 前缀（原型 :4217 `res-del`）。带 id ⇒ 武装 B 行会自动解除 A 行。 */
const RES_DEL_PREFIX = 'res-del:';

export function ResourcesScreen() {
  const { t } = useTranslation();
  const openDialog = useDialogStore((s) => s.open);
  const { armed, confirmTwice } = useConfirmTwice();
  const [source, setSource] = useState<SourceFilter>('all');
  const [items, setItems] = useState<RuleResourceListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<Record<string, RuleResourceProgress>>({});

  // 引用徽章 hover 卡（§1.5 refs）用：路由规则 + 应用分流 + 内置预设表，均已在 store，per-row 即时算即可。
  // 防御性兜底（IPC 边界不可信 TS 类型承诺）：即便 store 现状默认已是 []，仍显式 ?? []，避免 store 形态漂移时
  // resourceRefs 内部 for..of / .filter 在 undefined 上炸。
  const rules = useEffectiveRules() ?? [];
  const config = useEffectiveConfig();
  const builtinPresets = useAppPresetsStore((s) => s.presets) ?? [];
  const loadPresets = useAppPresetsStore((s) => s.loadPresets);
  useEffect(() => {
    void loadPresets();
  }, [loadPresets]);

  // 拉取资源列表
  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await api.ruleResources.list();
      // 真实健壮性 bug 修复：invoke() 的 TS 返回类型 RuleResourceListItem[] 只是编译期断言，
      // 后端在异常/未就绪路径可能实际下发 null/undefined（Tauri command 返回 Option::None 序列化即
      // null）。此前 setItems(list) 直接吞入非数组值，下方 filtered useMemo 对其 [...list]/.filter
      // 时以 "w is not iterable" / "Cannot read properties of undefined (reading 'servers')" 崩溃
      // ——在此收口成 []，渲染空态而非炸屏。
      setItems(Array.isArray(list) ? list : []);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(msg);
      console.error('[ResourcesScreen] list failed:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  // 订阅下载进度
  useEffect(() => {
    const off = api.ruleResources.onProgress((p) => {
      setProgress((prev) => ({ ...prev, [p.id]: p }));
      // cancelled 与 done/error 同属终态：都要清进度 + 重拉列表。漏掉它 → 取消后那行永远停在
      // spinner（后端已经中止了，界面却还在转圈 = 假进行中）。
      if (p.status === 'done' || p.status === 'error' || p.status === 'cancelled') {
        // 完成后清该条进度 + 重拉列表（拿到最新 size/downloadedAt）
        setTimeout(() => {
          setProgress((prev) => {
            const next = { ...prev };
            delete next[p.id];
            return next;
          });
          void reload();
        }, 300);
      }
    });
    return off;
  }, [reload]);

  // 源过滤 + 排序 + 分组（按 category 聚合，组内按 name 排序）。
  const filtered = useMemo(() => {
    // 双保险：items state 初值虽是 []，但 reload() 之外任何未来写路径万一塞入非数组，这里仍兜底，
    // 不依赖单一写入点自律。
    let list = Array.isArray(items) ? items : [];
    if (source === 'builtin') list = list.filter((r) => r.builtin);
    else if (source === 'external') list = list.filter((r) => !r.builtin);
    return [...list].sort((a, b) => a.name.localeCompare(b.name));
  }, [items, source]);

  const grouped = useMemo(() => {
    const map = new Map<RuleResourceCategory, RuleResourceListItem[]>();
    filtered.forEach((r) => {
      const arr = map.get(r.category) ?? [];
      arr.push(r);
      map.set(r.category, arr);
    });
    return [...map.entries()];
  }, [filtered]);

  // 更新族统一 reload：进度事件的 done/error 帧已会触发 reload，但那条路径只覆盖「至少产出了一个
  // 计划」的项——入参非法/不在册的项直接返错、不发进度帧，列表就再也不刷新了。故命令返回后兜一次。
  // 原型 :4200 res-update-all → notify('开始更新全部资源…')（进行中态）；逐项结果已由每行的
  // progress/errorState 就地反映，故这里只报「命令本身没能启动」的整体失败（catch），不重复逐项报。
  const handleUpdateAll = useCallback(async () => {
    toast.info(t('resources.updateAllStarted', '开始更新全部资源…'));
    try {
      await api.ruleResources.updateAll();
    } catch (err) {
      console.error('[ResourcesScreen] updateAll failed:', err);
      toast.error(t('resources.updateAllFailed', '更新失败'), err instanceof Error ? err.message : undefined);
    } finally {
      void reload();
    }
  }, [reload, t]);

  const handleUpdateOne = useCallback(
    async (item: RuleResourceListItem) => {
      try {
        // 内置 geo 与外置资源是**两条腿**：内置项从不入 `config.ruleResources`，走 `redownload(id)`
        // 恒返 `RULE_RESOURCE_NOT_FOUND`。内置腿按 tag（= `item.name`）取上游地址、原子换
        // `<userData>/rules/` 里的生效副本。
        if (item.builtin) await api.ruleResources.updateBuiltin(item.name);
        else await api.ruleResources.redownload(item.id);
      } catch (err) {
        console.error('[ResourcesScreen] update failed:', err);
      } finally {
        void reload();
      }
    },
    [reload],
  );

  /**
   * 删除单条资源 —— 原地二次点击（原型 :4217 `res-del`），**无条件**确认。
   *
   * 改形态前是「后端说要才确认」：先不带 force 探一次，只有被启用规则引用时后端才回 `needConfirm`
   * 并弹窗；**未被引用的最常见路径上确认整个不存在**（点一下即删）。原型那颗是无条件 confirmTwice，
   * 被引用时只是**换文案**，不是换有无。
   *
   * 因此第二下直接 `force:true`：确认已经在按钮上完成，后端那条 needConfirm 探测腿只服务于弹窗
   * 编排，留着会变成「确认两次」。删的是同一个东西（`plan_resource_delete` 的 force 分支只跳过
   * needConfirm 闸门，不改删除范围），语义不变。
   *
   * 「被 N 条规则引用」这份信息不随弹窗一起消失：行上常驻 `RefBadge`（数字 + hover 明细），
   * 武装态的 title 再复述一次原型那句警告 —— 从「点开才看得见」变成「一直看得见」。
   */
  const handleDeleteOne = useCallback(
    (item: RuleResourceListItem) => {
      confirmTwice(`${RES_DEL_PREFIX}${item.id}`, () => {
        void (async () => {
          try {
            await api.ruleResources.delete(item.id, true);
            // 原型 :4217 res-del → notify('资源已删除')（中性 kind，非 'ok'）。
            toast.info(t('resources.deleteDone', '资源已删除'));
          } catch (err) {
            console.error('[ResourcesScreen] delete failed:', err);
            toast.error(t('resources.deleteFail', '删除失败'));
          } finally {
            void reload();
          }
        })();
      });
    },
    [confirmTwice, reload, t],
  );

  /**
   * 取消在途下载（原型 :5376 `res-cancel`）。后端 `rule_resources_cancel` 用 `select!` 丢弃传输
   * future → 真中断连接，不落盘不入册（见 commands/rules.rs `download_with_progress` 的取消段）。
   *
   * `cancelled === 0` = 点下去时下载恰好已结束（竞态）→ 如实告知「已完成，无需取消」，不静默。
   * 终态帧（`status:'cancelled'`）由后端发回，onProgress 那条腿负责清行 + reload。
   */
  const handleCancelOne = useCallback(
    async (item: RuleResourceListItem) => {
      try {
        const r = await api.ruleResources.cancel(item.id);
        if (!r || r.cancelled === 0) {
          toast.info(t('resources.cancelAlreadyDone', '下载已结束，无需取消'));
          void reload();
        }
      } catch (err) {
        console.error('[ResourcesScreen] cancel failed:', err);
        toast.error(t('resources.cancelFailed', '取消失败'), err instanceof Error ? err.message : undefined);
      }
    },
    [reload, t],
  );

  /** 重置内置资源 —— 原地二次点击（原型 :4198 `geo-reset`）。此前**零闸门**：一键覆盖 geosite + geoip
   *  两类全部内置资源，点错没有任何拦截。 */
  const handleResetBuiltin = useCallback(() => {
    confirmTwice(GEO_RESET_KEY, () => {
      void (async () => {
        // 单一「重置内置资源」按钮 = 重置**全部**内置资源（geosite + geoip 两类）。
        // 传 'geosite' 只命中后端的 geosite 分支 → geoip-cn 等内置 GeoIP 项被静默漏掉，按钮名不副实。
        // 后端 rule_resources_reset_builtin 对无法识别的 tag 走 `_ => None` = 两类全重置，故传 'all'。
        try {
          await api.ruleResources.resetBuiltin('all');
          // 原型 :4198 geo-reset → notify('内置资源已重置','ok')。
          toast.success(t('resources.resetBuiltinDone', '内置资源已重置'));
        } catch (err) {
          console.error('[ResourcesScreen] resetBuiltin failed:', err);
          toast.error(t('resources.resetBuiltinFailed', '重置失败'), err instanceof Error ? err.message : undefined);
        } finally {
          void reload();
        }
      })();
    });
  }, [confirmTwice, reload, t]);

  return (
    <section id="s-resources" className="screen">
      <div className="phead">
        <div>
          <h1>{t('sidebar.ruleResources', '规则资源')}</h1>
          <div className="sub">
            {t(
              'resources.sub',
              '供路由规则引用的 .srs 规则集',
            )}
          </div>
        </div>
        <div className="acts">
          <button
            type="button"
            className={cn('btn ghost', armed === GEO_RESET_KEY && 'confirming')}
            onClick={handleResetBuiltin}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M4 4v6h6M20 20v-6h-6" />
              <path d="M4 10a8 8 0 0114-3M20 14a8 8 0 01-14 3" />
            </svg>
            <span>
              {armed === GEO_RESET_KEY
                ? t('resources.resetBuiltinConfirm', '重置内置资源？')
                : t('resources.resetBuiltin', '重置内置资源')}
            </span>
          </button>
          <button
            type="button"
            className="btn ghost"
            onClick={() => openDialog({ kind: 'res-catalog' })}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M4 6h16M4 12h16M4 18h10" />
            </svg>
            <span>{t('resources.catalog', '资源库')}</span>
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => openDialog({ kind: 'res-url' })}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M9 15l6-6M8 8a3 3 0 10-3 3" />
            </svg>
            <span>{t('resources.urlDownload', 'URL 下载')}</span>
          </button>
        </div>
      </div>

      <div className="card-h" style={{ marginBottom: 12 }}>
        {t('resources.downloaded', '已下载资源')}
      </div>

      {/* 源过滤 + 全部更新（原型同排） */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          gap: 12,
          marginBottom: 12,
        }}
      >
        <div
          className="seg2"
          role="group"
          aria-label={t('resources.srcFilter', '资源来源')}
          style={{ display: 'flex', maxWidth: 340, flex: '0 1 auto' }}
        >
          {(['all', 'builtin', 'external'] as SourceFilter[]).map((s) => (
            <button
              key={s}
              type="button"
              className={cn(source === s && 'on')}
              style={{ flex: 1 }}
              onClick={() => setSource(s)}
            >
              {s === 'all'
                ? t('resources.src.all', '全部')
                : s === 'builtin'
                  ? t('resources.src.builtin', '内置')
                  : t('resources.src.external', '外置')}
            </button>
          ))}
        </div>
        <button type="button" className="btn ghost sm" style={{ flex: 'none' }} onClick={handleUpdateAll}>
          <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M4 4v6h6M20 20v-6h-6" />
            <path d="M4 10a8 8 0 0114-3M20 14a8 8 0 01-14 3" />
          </svg>
          <span>{t('resources.updateAll', '全部更新')}</span>
        </button>
      </div>

      {/* 资源表 */}
      <div className="res-table">
        <div className="res-row head">
          <span>{t('resources.col.name', '名称')}</span>
          <span>{t('resources.col.size', '大小')}</span>
          <span className="rc-hide">{t('resources.col.updated', '更新于')}</span>
          <span />
        </div>

        {loading ? (
          <div className="res-row">
            <span className="res-name">
              <span className="spinner" />
              {t('resources.loading', '加载中…')}
            </span>
            <span />
            <span className="rc-hide" />
            <span />
          </div>
        ) : error ? (
          <div className="res-row">
            <span className="res-name" style={{ color: 'hsl(var(--err))' }}>
              {t('resources.loadError', '加载失败')}：{error}
            </span>
            <span />
            <span className="rc-hide" />
            <span />
          </div>
        ) : grouped.length === 0 ? (
          <div className="res-row">
            <span className="res-name">
              {t('resources.empty', '暂无已下载资源')}
            </span>
            <span />
            <span className="rc-hide" />
            <span />
          </div>
        ) : (
          // 原型 :1986-1995 res-grp/res-row 是 .res-table 的**扁平兄弟**（非嵌套包一层）。之前用
          // <div key={cat}> 包一层分组：CSS `.res-grp:not(:first-child){border-top}` 判的是
          // "是否为其真实 DOM 父元素的第一个子节点"——包一层后每个 .res-grp 都变成了自己那层 wrapper
          // 的唯一/第一个子节点，规则永远不命中，组间分隔线（含首组紧邻表头那条）全部丢失。改用
          // Fragment 保持扁平，才能让首组也命中 :not(:first-child)（首子节点是 .res-row.head，
          // 首个 .res-grp 排第二位，原型demo正是如此）。
          grouped.map(([cat, catItems]) => (
            <Fragment key={cat}>
              <div className="res-grp">
                <span>{categoryLabel(cat)}</span>
                <span className="grp-count mono">{catItems.length}</span>
              </div>
              {catItems.map((item) => (
                <ResRow
                  key={item.id}
                  item={item}
                  progress={progress[item.id]}
                  deleteConfirming={armed === `${RES_DEL_PREFIX}${item.id}`}
                  onUpdate={() => handleUpdateOne(item)}
                  onCancel={() => handleCancelOne(item)}
                  onDelete={() => handleDeleteOne(item)}
                  refs={resourceRefs(
                    item.id,
                    rules,
                    config?.appRules ?? [],
                    builtinPresets,
                    config?.customAppPresets,
                  )}
                />
              ))}
            </Fragment>
          ))
        )}
      </div>
    </section>
  );
}

/** 单行资源（含引用徽章 + 更新/进度态）。 */
function ResRow({
  item,
  progress,
  deleteConfirming,
  onUpdate,
  onCancel,
  onDelete,
  refs,
}: {
  item: RuleResourceListItem;
  progress?: RuleResourceProgress;
  deleteConfirming: boolean;
  onUpdate: () => void;
  onCancel: () => void;
  onDelete: () => void;
  refs: ResourceRef[];
}) {
  const { t } = useTranslation();
  const downloading = progress?.status === 'downloading';
  const errorState = progress?.status === 'error';
  // 取消是终态但**不是失败**：不用 warn 配色，也不说「更新失败」（源没挂，是用户自己停的）。
  const cancelledState = progress?.status === 'cancelled';
  /** 武装态的删除提示语 —— 被引用时用原型 :4217 那句带条数的警告，未引用时用「删除该资源？」。 */
  const deleteLabel = !deleteConfirming
    ? t('common.delete', '删除')
    : item.referencedBy > 0
      ? t('resources.deleteConfirmRefd', {
          defaultValue: '正被 {{count}} 项规则引用，删除后将暂时失效（重新下载后自动恢复）',
          count: item.referencedBy,
        })
      : t('resources.deleteConfirmPlain', '删除该资源？');
  const updatedText = errorState
    ? t('resources.updateFailed', '更新失败')
    : cancelledState
      ? t('resources.cancelled', '已取消')
      : downloading && progress?.percent != null
        ? `${Math.floor(progress.percent)}%`
        : relativeTime(item.downloadedAt);

  return (
    <div className="res-row">
      {/* 原型 :1987 res-name 首子节点是裸文本节点（"category-ads-all "）非 <span> 包裹，
          逐字对齐——.res-name flex+gap:7px 对文本节点/span 视觉等价，但不额外包元素。 */}
      <span className="res-name">
        {item.name}
        <span className="pill region">
          {item.builtin ? t('resources.src.builtin', '内置') : t('resources.src.external', '外置')}
        </span>
        <RefBadge item={item} refs={refs} />
      </span>
      <span className="mono">{fmtBytes(item.size)}</span>
      <span
        className={cn('rc-hide', errorState && 'warn')}
        style={errorState ? { color: 'hsl(var(--warn))' } : undefined}
      >
        {downloading && progress?.percent != null ? (
          // 原型 :1992 .res-prog style="width:120px"（geoip-cn 演示行）——逐字带上，非交给 flex 默认撑满。
          <div className="res-prog" style={{ width: 120 }}>
            <span className="bar">
              <i style={{ width: `${progress.percent}%` }} />
            </span>
            <span>{updatedText}</span>
          </div>
        ) : (
          updatedText
        )}
      </span>
      {/* 原型 :1987-1995 每条数据行第 4 列 <span style="text-align:right"> 逐字带上
          （表头 :1984 该列为裸 <span></span> 无此样式，故仅数据行加）。 */}
      <span style={{ textAlign: 'right' }}>
        {downloading ? (
          /* 原型 :5376 下载中格：spinner + 「取消」按钮（enhanceResRows 亦在 spinner 旁 append
             data-act="res-cancel"）。后端 rule_resources_cancel 真中断传输，非仅隐藏 spinner。 */
          <>
            <span className="spinner" style={{ width: 14, height: 14 }} />
            <button
              type="button"
              className="nd-a res-cancel"
              style={{ marginLeft: 6 }}
              onClick={onCancel}
              data-tip={t('resources.cancel', '取消')}
              aria-label={t('resources.cancel', '取消')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M5 5l14 14M19 5L5 19" />
              </svg>
            </button>
          </>
        ) : (
          <>
            <button
              type="button"
              className="nd-a"
              style={errorState ? { color: 'hsl(var(--warn))' } : undefined}
              onClick={onUpdate}
              data-tip={errorState ? t('resources.retryNow', '立即重试') : t('resources.update', '更新')}
              aria-label={errorState ? t('resources.retryNow', '立即重试') : t('resources.update', '更新')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M4 4v6h6" />
                <path d="M4 10a8 8 0 0114-3" />
              </svg>
            </button>
            {/* 内置 geo 被智能分流隐式依赖、后端 `rule_resources_delete` 对其也无路径 ⇒ **入口不渲染**
                （而非渲染后禁用：一颗点了必然失败的按钮比没有更坏）。要复原走页头「重置内置资源」。
                确认 = 原地二次点击（原型 :4217）：图标钮无 `<span>` 可换文案 ⇒ 只靠 `.nd-a.confirming`
                翻红 + title/aria-label 换成提示语（纯颜色状态对键盘/读屏用户不可达，同 NodeCard）。
                被规则引用时换成原型那句更重的警告 —— 原型只换文案、不换有无确认。 */}
            {!item.builtin && (
              <button
                type="button"
                className={cn('nd-a err', deleteConfirming && 'confirming')}
                onClick={onDelete}
                data-tip={deleteLabel}
                aria-label={deleteLabel}
              >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <path d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13" />
                </svg>
              </button>
            )}
          </>
        )}
      </span>
    </div>
  );
}

/**
 * 引用徽章：referencedBy>0（链接+数，hover 弹引用明细）/ 0（dash 未引用，原生 title——原型 :1992
 * 该态本就走纯文本 data-tip，不挂 tipcard）。hover 卡见 §1.5 refs（ResourceRefsHoverCard.tsx）。
 *
 * 内置 geo 单独一档 `ref-badge sys`（见下）—— 它们被智能分流**隐式**引用，`referencedBy` 实算恒 0，
 * 落到「未引用」那档会误导用户去删（而删不掉）。
 */
function RefBadge({ item, refs }: { item: RuleResourceListItem; refs: ResourceRef[] }) {
  const { t } = useTranslation();
  const hc = useHoverCard<HTMLSpanElement>();
  if (item.referencedBy > 0) {
    return (
      <span
        className="ref-badge"
        ref={hc.triggerRef}
        {...hc.triggerHandlers}
        aria-label={t('resources.refCountAria', '{{count}} 条规则引用', { count: item.referencedBy })}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2}>
          <path d="M10 13a5 5 0 007 0l2-2a5 5 0 00-7-7l-1 1" />
          <path d="M14 11a5 5 0 00-7 0l-2 2a5 5 0 007 7l1-1" />
        </svg>
        <span>{item.referencedBy}</span>
        <HoverCardPanel
          cardRef={hc.cardRef}
          open={hc.open}
          pos={hc.pos}
          onMouseEnter={hc.cardHandlers.onMouseEnter}
          onMouseLeave={hc.cardHandlers.onMouseLeave}
        >
          <ResourceRefsHoverCardContent refs={refs} />
        </HoverCardPanel>
      </span>
    );
  }
  if (item.builtin) {
    // 内置 geo 被智能分流隐式引用（globe 图标）——refs 到这一分支恒为空（referencedBy===0），
    // 合成一条系统行，与既有 sys 徽标「智能分流」同一口径（非新增判定）。
    return (
      <span
        className="ref-badge sys"
        ref={hc.triggerRef}
        {...hc.triggerHandlers}
        aria-label={t('resources.smartRouting', '智能分流')}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <circle cx="12" cy="12" r="9" />
          <path d="M3 12h18M12 3c3 3 3 15 0 18" />
        </svg>
        <HoverCardPanel
          cardRef={hc.cardRef}
          open={hc.open}
          pos={hc.pos}
          onMouseEnter={hc.cardHandlers.onMouseEnter}
          onMouseLeave={hc.cardHandlers.onMouseLeave}
        >
          <ResourceRefsHoverCardContent
            refs={[{ kind: 'system', label: t('resources.refSystemBaseline', '智能分流基线') }]}
          />
        </HoverCardPanel>
      </span>
    );
  }
  // 未引用（dash，原型 :1992 走纯文本 data-tip，不挂 tipcard）。
  // **注意 data-tip ≠ 原生 title**：原型 :195-198 的署名决定写着这套引擎存在的目的就是**取代**
  // 原生 title=（§4 migration）。此处原注释曾把两者写成「对齐」，那条错误等价关系一度成了全仓
  // 112 处沿用原生 title 的隐性依据（2026-07-29 二轮取证 §10.4）。纯文本档走引擎，富卡片档走
  // HoverCard —— 两档都不是原生 title。
  // 补 aria-label 对齐原型 aria-label="Unreferenced" 的存在性）
  return (
    <span className="ref-badge none" data-tip={t('resources.unreferenced', '未引用')} aria-label={t('resources.unreferenced', '未引用')}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2}>
        <path d="M6 12h12" />
      </svg>
    </span>
  );
}

export default ResourcesScreen;
