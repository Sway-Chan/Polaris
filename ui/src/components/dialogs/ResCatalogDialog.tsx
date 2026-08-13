/**
 * ResCatalogDialog —— 资源库弹窗（原型 #res-catalog-dialog :2695-2719，cat* :5306-5369）。
 *
 * 内置/外置 tab + 搜索 + 多选行 + 批量下载。
 *
 * 后端现状（据实呈现，不假装两套清单）：
 *  - `api.ruleResources.getCatalog()` 是**真·内置清单**（Rust SoT，28 条 = 随包表的投影），驱动
 *    「内置」tab —— 投影 ⇒ 本 tab 每一行恒 `bundled:true`，故这里永远标不出「已下载」；
 *  - `api.ruleResources.refreshCatalog()` **真拉 meta-rules-dat 全量清单**（GitHub git-trees API 三跳
 *    → 原子落缓存 `<userData>/rule-resource/catalog.json`），驱动「外置」tab 的「刷新清单」。三态
 *    `source` 各自对应一份真实清单：`remote`=本次拉到的、`cache`=上次拉到并落盘的、`builtin`=远端与
 *    缓存都拿不到时的内置精选回落。状态文案按真实 `source` 渲染，任何一态都不谎称「已从远程获取」；
 *  - `api.ruleResources.getCachedCatalog()` 零出站回读那份缓存，进弹窗即预载「外置」tab（真机
 *    2026-07-30：缓存一直在盘上，UI 却每次开都要求手点刷新 = 白付一次三跳往返）。预载与刷新在文案上
 *    分开：预载没打过网络，绝不能借「远程获取失败 · 使用本地缓存」那条文案报一个没发生过的失败；
 *  - 已具备的条目（随包出厂 / 已下载，判据见 `catalogItemStatus`）恒显勾选 + 名称后挂状态标签，
 *    且不计入下载目标 —— 勾选在这里是「你已经有了」的状态位，不是待下载选择；
 *  - 下载走 `api.ruleResources.download`——**真下载**（SSRF-safe 拉取 + 校验 + 落盘 + upsert config，同 ResUrlDialog）。
 *    结果与入参逐项同序，逐项独立容错；失败项汇总报错，弹窗保持打开。
 *
 * 列表怎么排、显示谁、空了说什么 —— 三条判据全部落在 `domain/rule-resource-catalog.ts` 的纯函数里
 * （`catalogTabItems` / `catalogEmptyKind`），本文件只接线。理由同 `catalogItemStatus`：本仓 vitest
 * 是 node 环境无 jsdom，判据留在 JSX 三元里等于没有门。
 */

import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { api } from '@/ipc';
import type { RuleResourceCatalogItem, RuleResourceCatalogResult } from '@/contracts/types';
import {
  categoryLabel,
  catalogItemStatus,
  catalogTabItems,
  catalogEmptyKind,
} from '@/domain/rule-resource-catalog';
import { cn } from '@/lib/utils';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';

type CatTab = 'builtin' | 'external';

/**
 * 外置清单来源：后端三态 + `preload`（进弹窗零出站读到的盘上缓存）。
 * `preload` 必须与后端的 `cache` 分开：后者是「远程拉了但失败，回落缓存」，前者根本没拉过。
 */
type ExtSource = RuleResourceCatalogResult['source'] | 'preload';

function CatalogIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M4 6h16M4 12h16M4 18h10" />
    </svg>
  );
}

export function ResCatalogDialog() {
  const { t } = useTranslation();
  const close = useDialogStore((s) => s.close);

  const [tab, setTab] = useState<CatTab>('builtin');
  const [builtin, setBuiltin] = useState<RuleResourceCatalogItem[] | null>(null);
  const [external, setExternal] = useState<RuleResourceCatalogItem[] | null>(null);
  const [extSource, setExtSource] = useState<ExtSource | null>(null);
  const [extLoading, setExtLoading] = useState(false);
  const [localIds, setLocalIds] = useState<Set<string>>(new Set());
  const [search, setSearch] = useState('');
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [downloading, setDownloading] = useState(false);
  // 两处失败**分开存**，不合成一个 err：初始加载失败让两个 tab 都空（内置清单没拿到、预载被
  // Promise.all 一并丢弃），刷新失败只影响外置。合成一个的话，外置刷新成功会顺手清掉内置那条
  // 解释，而内置 tab 明明还是空的 —— 又退回「对着空列表没有解释」。
  const [loadErr, setLoadErr] = useState<string | null>(null);
  const [extErr, setExtErr] = useState<string | null>(null);

  const reason = (e: unknown) => (e instanceof Error ? e.message : String(e));

  // 进弹窗即拉：内置精选清单（驱动「内置」tab）+ 已下载资源 id 集合 + 盘上的外置清单缓存。
  // 抽成具名函数（而非留在 effect 里）是因为失败空态要给重试入口，重试走的必须是同一条腿。
  const loadInitial = async (alive: () => boolean = () => true) => {
    setLoadErr(null);
    try {
      const [catalog, resources, cached] = await Promise.all([
        api.ruleResources.getCatalog(),
        api.ruleResources.list(),
        // 预载是锦上添花，失败**不得**连累前两项：它一 reject，Promise.all 会把整个弹窗打成错误态
        // （内置 tab 一并空掉）。就地吞掉，外置 tab 退回「点击刷新清单」即可。
        api.ruleResources.getCachedCatalog().catch(() => null),
      ]);
      if (!alive()) return;
      setBuiltin(catalog.items);
      setLocalIds(new Set(resources.filter((r) => r.fileExists).map((r) => r.id)));
      if (cached) {
        setExternal(cached.items);
        setExtSource('preload');
      }
    } catch (e) {
      // 不走 toast：这不是提交触发（用户没点任何按钮，是开弹窗即拉），2.2s 后 toast 消失，
      // 用户对着空列表再无任何解释。持久空态见下方 `catalogEmptyKind` 的 'error' 分支。
      if (alive()) setLoadErr(reason(e));
    }
  };

  useEffect(() => {
    let alive = true;
    void loadInitial(() => alive);
    return () => {
      alive = false;
    };
  }, []);

  const refreshExternal = async () => {
    setExtLoading(true);
    setExtErr(null);
    try {
      const catalog = await api.ruleResources.refreshCatalog();
      setExternal(catalog.items);
      setExtSource(catalog.source);
    } catch (e) {
      // 同上不走 toast。两条腿承接它：列表空 → 空态给原因 + 重试；列表非空（上一份缓存还在）
      // → 空态压根不渲染，故 `extStatusText()` 也要报失败，否则用户点了刷新什么反馈都没有。
      setExtErr(reason(e));
    } finally {
      setExtLoading(false);
    }
  };

  const setTabAndClear = (v: CatTab) => {
    if (tab === v) return;
    setTab(v);
    setSel(new Set());
  };

  // 排序/去重放在搜索**之前**这一层：外置全量 2000+ 条，跟着每次输入重排是白烧。
  // 先排后过滤与先过滤后排的相对序等价（过滤不改变相对序）。
  const items = useMemo(
    () => catalogTabItems(tab, builtin ?? [], external ?? [], localIds),
    [tab, builtin, external, localIds],
  );
  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return q ? items.filter((it) => it.name.toLowerCase().includes(q)) : items;
  }, [items, search]);

  // 当前 tab 该为哪条失败负责：外置 tab 上刷新失败优先于初始加载失败（后者更旧）。
  const tabErr = tab === 'external' ? extErr ?? loadErr : loadErr;
  const emptyKind = catalogEmptyKind({
    error: tabErr,
    notFetched: tab === 'external' && external == null,
    total: items.length,
    count: filtered.length,
  });
  // 重试打哪条腿：刷新失败重刷清单，其余（含外置 tab 上的初始加载失败）重跑初始加载 ——
  // 初始加载还带 `list()`，只刷清单的话已下载状态仍是空的，标签会全错。
  const retryEmpty = () => {
    if (tab === 'external' && extErr !== null) void refreshExternal();
    else void loadInitial();
  };

  // LOW-7：实际下载目标 = filtered ∩ selected ∩ 未具备——按钮计数/可用态须与此一致，
  // 否则搜索把已选项过滤出视口后，按钮仍显旧计数且可点，点击却静默空跑。
  // 「未具备」含随包出厂项：它们恒显勾选（状态位，非待下载选择），若计入目标，点「下载选中」会去
  // 下一份 route.rs 恒不采用的副本（随包在位时优先注入随包那份）= 纯白下。
  const downloadTargets = useMemo(
    () => filtered.filter((it) => sel.has(it.id) && catalogItemStatus(it, localIds) === null),
    [filtered, sel, localIds],
  );

  const toggle = (id: string) => {
    setSel((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  // 文案 = 来源的直译。后端三态里的 `cache`/`builtin` 只会在**远程拉取失败**时出现（成功恒为
  // `remote`），故那两条点明失败原因，不让用户以为「刷新了但没变化」；而 `preload` 是本地预载、
  // 一次网都没打，单列一条，绝不借用「远程获取失败」的说法。
  const extStatusText = (): string => {
    if (extLoading) return t('resCatalog.extFetching', '获取中…');
    // 刷新失败排在 extSource 之前：失败时 extSource 停在上一次成功的值，照它渲染会说
    // 「已从远程获取最新清单」—— 用户刚点了刷新、什么都没变，还被告知拉成功了。
    if (extErr !== null) return t('resCatalog.loadFailed', '清单加载失败');
    if (extSource == null) return t('resCatalog.extNotFetched', '尚未获取清单');
    switch (extSource) {
      case 'preload':
        return t('resCatalog.extPreloaded', '本地缓存清单 · 点「刷新清单」更新');
      case 'remote':
        return t('resCatalog.extRemote', '已从远程获取最新清单');
      case 'cache':
        return t('resCatalog.extCache', '远程获取失败 · 使用本地缓存清单');
      default:
        return t('resCatalog.extBuiltin', '远程获取失败 · 回落内置精选清单');
    }
  };

  const handleDownload = async () => {
    if (downloadTargets.length === 0) return;
    setDownloading(true);
    try {
      const results = await api.ruleResources.download(
        downloadTargets.map((it) => ({ catalogId: it.id, name: it.name, category: it.category })),
      );
      if (results.length === 0) {
        toast.error(t('resCatalog.errUnavailable', '下载后端尚未接入（需 HTTP 下载能力）'));
        return;
      }
      const failed = results.filter((r) => !r.ok);
      if (failed.length > 0) {
        toast.error(
          t('resCatalog.downloadAllFailed', '下载失败'),
          failed.map((r) => r.error ?? r.errorCode ?? 'download failed').join('; '),
        );
        return;
      }
      close();
    } catch (e) {
      toast.error(t('resCatalog.downloadAllFailed', '下载失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setDownloading(false);
    }
  };

  return (
    <Modal
      titleId="cat-dlg-title"
      title={t('resCatalog.title', '资源库')}
      onClose={close}
      icon={<CatalogIcon />}
      style={{ width: 'min(560px, 100%)' }}
      footer={
        <>
          <button type="button" className="btn ghost" onClick={close}>
            {t('common.cancel', '取消')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => void handleDownload()}
            disabled={downloadTargets.length === 0 || downloading}
          >
            {/* 计数**恒渲染**（含 `(0)`），不要改回 `length > 0 &&` 条件渲染：条件渲染会让按钮
                在勾选↔取消勾选之间变宽变窄（实测 78.906px ↔ 53.016px，差 25.89px），而
                `.dlg-foot` 是 `justify-content:flex-end` ⇒ 右边界钉死、**左边界横移** 25.89px。
                2026-07-30 mac 真机报的「取消按钮与下载按钮之间残留上一次的实体蓝 + 下 字」正是
                这条让出来的竖条没被重绘（旧按钮左半截的底色与首字）。恒渲染后 0~9 选中项宽度
                **逐像素相同**（数字等宽，五语实测一致），报上来的那条路径不再有几何变化。
                本机 WebKitGTK 4.1 未能复现该残留（终态帧与「直接以终态首屏渲染」逐像素全等），
                故此为防御性修法 —— 真机复验前别当已证实。门见 style-invariants.test.ts。 */}
            <span>{t('resCatalog.download', '下载')}</span>
            <span>&nbsp;({downloadTargets.length})</span>
          </button>
        </>
      }
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
        <div className="cat-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'builtin'}
            className={cn(tab === 'builtin' && 'on')}
            onClick={() => setTabAndClear('builtin')}
          >
            {t('resCatalog.builtin', '内置')}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'external'}
            className={cn(tab === 'external' && 'on')}
            onClick={() => setTabAndClear('external')}
          >
            {t('resCatalog.external', '外置')}
          </button>
        </div>
        <label className="input search-box" style={{ flex: '1 1 150px', minWidth: 150 }}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} style={{ width: 15 }}>
            <circle cx="11" cy="11" r="7" />
            <path d="M20 20l-3-3" />
          </svg>
          <input
            type="search"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t('resCatalog.searchPh', '搜索资源…')}
          />
        </label>
      </div>

      {tab === 'external' && (
        <div className="cat-ext-bar">
          <button type="button" className="btn ghost sm" onClick={() => void refreshExternal()} disabled={extLoading}>
            <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M4 4v6h6M20 20v-6h-6" />
              <path d="M4 10a8 8 0 0114-3M20 14a8 8 0 01-14 3" />
            </svg>
            <span>{t('resCatalog.refreshList', '刷新清单')}</span>
          </button>
          <span className="cat-ext-status">{extStatusText()}</span>
        </div>
      )}

      <div className="cat-list">
        {/* 加载失败态：**持久**留在列表位（形态复用 AppAddDialog 图标库那处「失败 + 原因 + 重试」，
            不另起样式）。此前这条走 toast —— 但 toast 是给「提交触发」用的（用户点了按钮，注意力
            在，2.2s 够用），而清单是开弹窗即拉的，闪一下就没了，用户对着空列表毫无解释。 */}
        {emptyKind === 'error' ? (
          <div className="cat-empty">
            <div>{t('resCatalog.loadFailed', '清单加载失败')}</div>
            <div>{tabErr}</div>
            <button
              type="button"
              className="btn ghost sm"
              style={{ marginTop: 8 }}
              onClick={retryEmpty}
            >
              {t('common.retry', '重试')}
            </button>
          </div>
        ) : emptyKind === 'notFetched' ? (
          <div className="cat-empty">{t('resCatalog.clickRefresh', '点击「刷新清单」获取资源清单')}</div>
        ) : emptyKind === 'noMatch' ? (
          <div className="cat-empty">{t('resCatalog.noMatch', '无匹配资源')}</div>
        ) : (
          filtered.map((it) => {
            const status = catalogItemStatus(it, localIds);
            // 已具备（随包/已下载）= 不可再操作，且**恒显勾选**：用户诉求「已内置/已下载的默认就该是
            // 勾上的」，勾选在这一行的语义是「你已经有了」，不是「已加入下载队列」。
            const held = status !== null;
            const checked = held || sel.has(it.id);
            return (
              <div key={it.id} className={cn('cat-item', held && 'disabled')}>
                <span
                  className={cn('cat-ck', checked && 'on')}
                  role={held ? undefined : 'checkbox'}
                  aria-checked={held ? undefined : checked}
                  aria-disabled={held || undefined}
                  tabIndex={held ? undefined : 0}
                  onClick={() => !held && toggle(it.id)}
                  onKeyDown={(e) => {
                    if (!held && (e.key === 'Enter' || e.key === ' ')) {
                      e.preventDefault();
                      toggle(it.id);
                    }
                  }}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={3}>
                    <path d="M5 12l5 5 9-11" />
                  </svg>
                </span>
                <span className="cat-nm">{it.name}</span>
                {/* 状态标签紧跟名称（真机诉求：状态要贴着规则集名，而不是甩在行尾），两种状态共用
                    同一个 .cat-badge —— 它们是同一件事的两个来源，没有第二套样式的理由。 */}
                {status !== null && (
                  <span className="cat-badge">
                    {status === 'bundled'
                      ? t('resCatalog.bundled', '已内置')
                      : t('resCatalog.downloaded', '已下载')}
                  </span>
                )}
                <span className="cat-meta">
                  <span className={cn('pill', it.category.startsWith('geoip') ? 'region' : 'proto')}>
                    {categoryLabel(it.category)}
                  </span>
                </span>
              </div>
            );
          })
        )}
      </div>

      {tab === 'external' && external != null && filtered.length > 0 && (
        <div className="cat-count">
          {t('resCatalog.countLine', {
            defaultValue: '共 {{n}} 项 · {{dl}} 项可下载',
            n: filtered.length,
            // 「可下载」与勾选禁用面同一判据：随包出厂的也不该算进去，否则计数比实际能下的多。
            dl: filtered.filter((it) => catalogItemStatus(it, localIds) === null).length,
          })}
        </div>
      )}
    </Modal>
  );
}

export default ResCatalogDialog;
