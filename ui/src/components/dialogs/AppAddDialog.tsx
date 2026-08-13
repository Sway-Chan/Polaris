/**
 * AppAddDialog —— 添加自定义应用弹窗（原型 #app-add-dialog :2589-2656，aad* :4634-4762）。
 *
 * 图标四子面板（首字母/在线图库/URL/emoji）各自持有状态；预览统一走「当前方法的图片源 → onerror
 * 隐藏 → 回退首字母」的单一渲染分支（同 AppPolicyScreen.AppIcon 的 onerror 降级语言，非新发明）。
 * `api.ruleResources.fetchIconGalleries` 现真拉取（后端并发拉 Qure+edc 两图库源各三镜像回退，见
 * `commands/rules.rs`）→ 图库网格是主路径。三态分流：loading / error（可重试）/ 真的空 才引导改用 URL
 * 面板（用户明确要求的降级路径），不是无条件降级；URL 面板本身也是有效 fallback + 用户主动选择，保留。
 *
 * geosite/geoip 标签池 = **内置清单（=随包）∪ 已下载资源**，而非硬编码列表。两者都要：内置清单
 * 现已收敛成随包表的投影（不再含 geoip-jp / geosite-apple 这类只列不随包的条目），只取它就等于把
 * 用户从「外置」tab 下回来的资源挡在选择池外 —— 下得到、选不上。反过来只取已下载也不行：随包项
 * 在盘上但不入 `config.ruleResources`，`list()` 里靠 `builtin:` 前缀那批才带得出来。
 * 「本地是否已下载」用 `api.ruleResources.list()` + `domain/rule-resource-refs.availableResourceTagSet`
 * （既有正向判定，D5 直接复用，非重新实现）。选中标签缺失本地资源时仅作提醒（不阻断提交——提交
 * 只是写 config，不触发下载）。
 *
 * 「从进程选择」嵌套 proc-pick（D4 并发批，当前 stub 渲染 null）：union kind + onPick 回调已冻结在
 * dialog-store，这里 open({kind:'proc-pick', onPick}) 现在即可类型检查通过，D4 落地后自动可用。
 *
 * 提交：无编辑态（自定义应用仅新增）。写 `api.config.setValue('customAppPresets', [..., preset])`
 * （通用 KV 路径，无专用 command——契约里 customAppPresets 就是 UserConfig 的普通字段）。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { useNavStore } from '@/store/nav-store';
import { api } from '@/ipc';
import type { CustomAppPreset, SystemProcessInfo } from '@/contracts/types';
import { iconProxySrc } from '@/domain/icon-proxy';
import { availableResourceTagSet } from '@/domain/rule-resource-refs';
import { cn } from '@/lib/utils';
import { useScrollBatch } from '@/lib/use-scroll-batch';
import { editRoute } from '@/lib/staged-config';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { Modal } from './Modal';
import { Csel, type CselOption } from './Csel';
import { useDialogStore } from './dialog-store';

type IconMethod = 'letter' | 'online' | 'url' | 'emoji';
type TagKind = 'geosite' | 'geoip';

const DEFAULT_EMOJI = '🌐';
const EMOJI_PALETTE = ['🌐', '📺', '🎬', '🎮', '💬', '🤖', '🛒', '🎵'];
function AppAddIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <rect x="4" y="4" width="16" height="16" rx="2" />
      <path d="M12 9v6M9 12h6" />
    </svg>
  );
}

export function AppAddDialog() {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const closeAll = useDialogStore((s) => s.closeAll);
  /** 展示面：已有自定义应用列表（重名校验 + 下方直落盘腿的追加基准）。直落盘腿只在总开关关着时
   *  可达（`customAppPresets` 是 Class B），那时 effective 即入参本体 ⇒ 基准仍是磁盘那份。 */
  const config = useEffectiveConfig();
  const navigate = useNavStore((s) => s.navigate);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);

  // ── 基础字段 ──
  const [name, setName] = useState('');
  const [category, setCategory] = useState('video');
  const [customCategory, setCustomCategory] = useState('');
  const [proc, setProc] = useState('');
  const [errName, setErrName] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  // ── 图标 ──
  const [iconMethod, setIconMethod] = useState<IconMethod>('letter');
  const [onlineQuery, setOnlineQuery] = useState('');
  const [onlineSel, setOnlineSel] = useState<{ name: string; url: string } | null>(null);
  const [iconUrlInput, setIconUrlInput] = useState('');
  const [emoji, setEmoji] = useState('');
  const [galleries, setGalleries] = useState<Array<{ name: string; url: string }>>([]);
  // 四态：idle（还没拉，也不该拉）/ loading（拉取中）/ ready（拉到，可能是真的空）/ error（拉取失败）。
  // 旧的 galleriesLoaded 布尔无法区分「加载失败」与「真的空」——后端现真拉取，二者的引导语义不同
  // （失败可重试，真空才纯引导 URL）。
  // `idle` 是惰性加载的初态：弹窗默认落在「首字母」面板，在线图库是折叠的，此前却在 mount 时就无条件
  // 并发拉三个远程图库（Qure/homarr/edc ≈3100 项）——「默认折叠」形同装饰，绝大多数只想填个首字母
  // 图标的用户白白付了三次出站请求。现改为只有真的切到「在线图标」面板才拉。
  const [galleryStatus, setGalleryStatus] = useState<'idle' | 'loading' | 'ready' | 'error'>('idle');
  const [imgError, setImgError] = useState(false);

  // 卸载守卫：拉取是异步的，弹窗中途关闭时不得 setState（沿用旧 alive 语义，改用 ref 以便 retry 复用）。
  const mountedRef = useRef(true);
  useEffect(() => () => { mountedRef.current = false; }, []);

  /**
   * 图库图标本体现在有后端磁盘缓存（`<userData>/icons/remote/`，见 `icon_cache.rs`），命中即零出站。
   * 缓存无 TTL ⇒ 需要一个显式重拉口，就是这个计数器：`force` 刷新后 +1，拼进每个 `<img src>` 的
   * query 里。为什么不能只靠后端清缓存 —— 清的是后端磁盘那份，webview 自己那层对同一个 URL 的
   * 内存缓存不受影响，src 不变就可能一张都不重新请求，用户看到「点了刷新没变化」。
   * query 段不影响后端路由：`parse_route` 只读 `uri.path()`，解出的远端 URL 与缓存键都不变。
   */
  const [galleryBust, setGalleryBust] = useState(0);
  const bustSuffix = galleryBust > 0 ? `?r=${galleryBust}` : '';

  /** `force` = 用户点了「刷新」：走 refresh 命令（后端两层缓存一起作废）并顺带 bust 掉 webview 那层。 */
  const loadGalleries = useCallback(async (force = false) => {
    setGalleryStatus('loading');
    try {
      const list = force
        ? await api.ruleResources.refreshIconGalleries()
        : await api.ruleResources.fetchIconGalleries();
      if (mountedRef.current) {
        setGalleries(list);
        setGalleryStatus('ready');
        if (force) setGalleryBust((n) => n + 1);
      }
    } catch (e) {
      console.error('[AppAddDialog] fetchIconGalleries failed:', e);
      if (mountedRef.current) setGalleryStatus('error');
    }
  }, []);

  // 惰性：首次展开「在线图标」面板才拉。`idle` 门确保只触发一次——ready/error 都不再自动重拉
  // （error 由面板内的「重试」按钮显式驱动，否则来回切面板会变成隐式重试风暴）。
  useEffect(() => {
    if (iconMethod !== 'online' || galleryStatus !== 'idle') return;
    void loadGalleries();
  }, [iconMethod, galleryStatus, loadGalleries]);

  const previewSrc = useMemo(() => {
    if (iconMethod === 'online' && onlineSel) return iconProxySrc(onlineSel.url);
    if (iconMethod === 'url' && iconUrlInput.trim()) return iconProxySrc(iconUrlInput.trim());
    return '';
  }, [iconMethod, onlineSel, iconUrlInput]);

  useEffect(() => setImgError(false), [previewSrc]);

  const fallbackLetter = (name.trim()[0] || '?').toUpperCase();

  const filteredGalleries = useMemo(() => {
    const q = onlineQuery.trim().toLowerCase();
    return q ? galleries.filter((g) => g.name.toLowerCase().includes(q)) : galleries;
  }, [galleries, onlineQuery]);

  // homarr 源约 2800 图标 + Qure 310 ≈ 3100 项。filter 本身 O(n) 字符串匹配（几千项 < 1ms，无虞），
  // 但一次性渲染上千 <img> 会拖慢弹窗首帧（layout thrash）→ 分批渲染。判据（为什么是 scroll 事件
  // 而不是 loading="lazy" / IntersectionObserver）随实现搬到 `lib/use-scroll-batch.ts` 头注，
  // 规则弹窗的候选勾选区是第二个消费方 —— 两处共用一份，不留第二实现。
  const { count: renderCount, onScroll: onGridScroll } = useScrollBatch(
    filteredGalleries.length,
    onlineQuery,
  );
  const shownGalleries = useMemo(
    () => filteredGalleries.slice(0, renderCount),
    [filteredGalleries, renderCount],
  );

  // ── 资源标签（geosite 必选 ≥1 / geoip 可选）──
  const [tagKind, setTagKind] = useState<TagKind>('geosite');
  const [tagQuery, setTagQuery] = useState('');
  const [geositeSel, setGeositeSel] = useState<Set<string>>(new Set());
  const [geoipSel, setGeoipSel] = useState<Set<string>>(new Set());
  const [errGeosite, setErrGeosite] = useState(false);
  const [catalogTags, setCatalogTags] = useState<{ geosite: string[]; geoip: string[] }>({
    geosite: [],
    geoip: [],
  });
  const [available, setAvailable] = useState<Set<string>>(new Set());

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const [catalog, resources] = await Promise.all([
          api.ruleResources.getCatalog(),
          api.ruleResources.list(),
        ]);
        if (!alive) return;
        const byName = (a: string, b: string) => a.localeCompare(b);
        const avail = availableResourceTagSet(resources);
        // 池子按 kind 收：两侧都产出裸名（`youtube`），因为预设存的 geositeTags 就是裸名，
        // 带上 `geosite-` 前缀存进去会拼成 `geosite-geosite-youtube`。
        const poolOf = (kind: TagKind) => {
          // 内置侧只剩 geosite/geoip 两个 category（lite 从不随包，已不在内置清单里）；
          // 下载来的 lite 走 avail 那支，`geosite-lite-cn` → 裸名 `lite-cn`，回拼恰好还原。
          const names = catalog.items.filter((i) => i.category === kind).map((i) => i.name);
          for (const tag of avail) {
            if (tag.startsWith(`${kind}-`)) names.push(tag.slice(kind.length + 1));
          }
          return [...new Set(names)].sort(byName);
        };
        setCatalogTags({ geosite: poolOf('geosite'), geoip: poolOf('geoip') });
        setAvailable(avail);
      } catch (e) {
        console.error('[AppAddDialog] load catalog/resources failed:', e);
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const curTags = tagKind === 'geoip' ? catalogTags.geoip : catalogTags.geosite;
  const curSel = tagKind === 'geoip' ? geoipSel : geositeSel;
  const filteredTags = useMemo(() => {
    const q = tagQuery.trim().toLowerCase();
    return q ? curTags.filter((tg) => tg.toLowerCase().includes(q)) : curTags;
  }, [curTags, tagQuery]);

  const toggleTag = (kind: TagKind, tag: string) => {
    const setter = kind === 'geoip' ? setGeoipSel : setGeositeSel;
    setter((prev) => {
      const next = new Set(prev);
      if (next.has(tag)) next.delete(tag);
      else next.add(tag);
      return next;
    });
    if (kind === 'geosite') setErrGeosite(false);
  };

  const selectedChips = useMemo(
    () => [
      ...[...geositeSel].map((tag) => ({ kind: 'geosite' as const, tag })),
      ...[...geoipSel].map((tag) => ({ kind: 'geoip' as const, tag })),
    ],
    [geositeSel, geoipSel],
  );
  const missingChips = selectedChips.filter((c) => !available.has(`${c.kind}-${c.tag}`));

  const dirty = Boolean(
    name.trim() || geositeSel.size || geoipSel.size || proc.trim() || iconUrlInput.trim() || emoji || onlineSel,
  );

  // picker 是多选批量（原型「添加 N 项」），逐个按 path||name 去重（含批内互相去重）追加。
  const handleProcPick = (procs: SystemProcessInfo[]) => {
    setProc((cur) => {
      const list = cur
        .split(/[,\n]/)
        .map((s) => s.trim())
        .filter(Boolean);
      const seen = new Set(list.map((s) => s.toLowerCase()));
      for (const p of procs) {
        const val = p.path ?? p.name;
        const lv = val.toLowerCase();
        if (!seen.has(lv)) {
          seen.add(lv);
          list.push(val);
        }
      }
      return list.join(', ');
    });
  };

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('appAdd.discardTitle', '放弃添加？'),
        message: t('appAdd.discardMsg', '已填写的内容将不会保存。'),
        confirmLabel: t('node.discard', '放弃'),
        danger: true,
        onConfirm: () => {
          close();
          close();
        },
      },
    });
  };

  const handleSubmit = async () => {
    const trimmedName = name.trim();
    const nameEmpty = !trimmedName;
    const geositeEmpty = geositeSel.size === 0;
    setErrName(nameEmpty);
    setErrGeosite(geositeEmpty);
    if (nameEmpty || geositeEmpty) return;

    setSubmitting(true);
    try {
      const preset: CustomAppPreset = {
        id: `custom-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 7)}`,
        name: trimmedName,
        emoji: iconMethod === 'emoji' ? emoji || DEFAULT_EMOJI : fallbackLetter,
        geositeTags: [...geositeSel],
        category: category === 'custom' ? customCategory.trim() || 'custom' : category,
      };
      if (geoipSel.size > 0) preset.geoipTags = [...geoipSel];

      // 图标「设定即缓存」：确认在线图标（URL 面板 / 在线图库选中）时，此刻一次性下载到本地，
      // preset.iconUrl 存本地缓存 ref（polaris-icon://c/<file>），此后正常渲染零出站请求（隐私第一性）。
      // 只在这一「设定时刻」联网；缓存失败则回落存 remote URL（旧行为，不阻断添加）。
      const remoteIcon =
        iconMethod === 'online' && onlineSel
          ? onlineSel.url
          : iconMethod === 'url' && iconUrlInput.trim()
            ? iconUrlInput.trim()
            : null;
      if (remoteIcon) {
        try {
          preset.iconUrl = await api.icon.cacheAppIcon(preset.id, remoteIcon);
        } catch (e) {
          console.warn('[AppAddDialog] 图标缓存失败，回落存 remote URL：', e);
          preset.iconUrl = remoteIcon;
        }
      }

      const procNames = proc.trim()
        ? proc.split(/[,\n]/).map((s) => s.trim()).filter(Boolean)
        : [];
      if (procNames.length > 0) preset.processNames = procNames;

      // 配置暂存闸门：`customAppPresets` 是 UserConfig 字段（Class B），提交的就是**整个**
      // `CustomAppPreset`，天然满足重放要求的「幂等整体替换」。
      // 上面那次图标缓存不构成 W-3：它落的是本地图标缓存文件、不是配置的一部分，
      // 「重置」后至多留一个没人引用的缓存文件（不改变任何配置语义、也无远端效应）。
      if (editRoute('customAppPresets', stagingEnabled) === 'staged') {
        stage({
          id: `appPreset:${preset.id}`,
          kind: 'appPreset',
          label: `${t('appAdd.title', '添加自定义应用')} ${preset.name}`,
          entityPath: ['customAppPresets', preset.id],
          nextValue: preset,
        });
        close();
        return; // 零 IPC 写、零磁盘写（FR-1）
      }
      const next = [...(config?.customAppPresets ?? []), preset];
      await api.config.setValue('customAppPresets', next);
      // setValue 只写后端，不回流 store → 不补这一句，新增的应用不会出现在卡片墙上（表现成「添加没生效」）。
      // 同 AppPolicyScreen.removeCustomApp 的删除路径范式。
      useAppStore.setState((s) => (s.config ? { config: { ...s.config, customAppPresets: next } } : {}));
      close();
    } catch (e) {
      toast.error(t('common.saveFailed', '保存失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  const categoryOptions: CselOption[] = [
    { value: 'video', label: t('appPolicy.cat.video', '视频') },
    { value: 'social', label: t('appPolicy.cat.social', '社交') },
    { value: 'ai', label: 'AI' },
    { value: 'tools', label: t('appPolicy.cat.tools', '工具') },
    { value: 'game', label: t('appPolicy.cat.game', '游戏') },
    { value: 'custom', label: t('appAdd.catCustom', '自定义…') },
  ];

  return (
    <Modal
      titleId="aad-title"
      title={t('appAdd.title', '添加自定义应用')}
      onClose={requestClose}
      icon={<AppAddIcon />}
      footer={
        <>
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel', '取消')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => void handleSubmit()}
            disabled={submitting}
          >
            {t('appAdd.submit', '添加')}
          </button>
        </>
      }
    >
      {/* 图标 */}
      <div className="fld">
        <label className="fld-l">{t('appAdd.icon', '应用图标')}</label>
        <div className="aad-icon-row">
          <div className="aad-icon-preview">
            {iconMethod === 'emoji' ? (
              <span className="ico-fb">{emoji || DEFAULT_EMOJI}</span>
            ) : previewSrc && !imgError ? (
              <img className="app-ico-img" src={previewSrc} alt="" onError={() => setImgError(true)} />
            ) : (
              <span className="ico-fb">{fallbackLetter}</span>
            )}
          </div>
          <div className="aad-icon-methods">
            <div className="seg2" role="group" aria-label="Icon method" style={{ display: 'flex' }}>
              {(
                [
                  ['letter', t('appAdd.icoLetter', '首字母')],
                  ['online', t('appAdd.icoOnline', '在线图标')],
                  ['url', 'URL'],
                  ['emoji', 'Emoji'],
                ] as [IconMethod, string][]
              ).map(([m, label]) => (
                <button
                  key={m}
                  type="button"
                  style={{ flex: 1 }}
                  className={cn(iconMethod === m && 'on')}
                  onClick={() => setIconMethod(m)}
                >
                  {label}
                </button>
              ))}
            </div>

            {iconMethod === 'online' && (
              <div className="aad-ico-pane">
                <div style={{ display: 'flex', gap: 8, marginTop: 8, alignItems: 'stretch' }}>
                  <label className="input search-box" style={{ flex: 1 }}>
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} style={{ width: 14 }}>
                      <circle cx="11" cy="11" r="7" />
                      <path d="M20 20l-3-3" />
                    </svg>
                    <input
                      value={onlineQuery}
                      onChange={(e) => setOnlineQuery(e.target.value)}
                      placeholder={t('appAdd.icoSearchPh', '搜索图标…（chatgpt / netflix …）')}
                    />
                  </label>
                  {/* 刷新 = 整份重来：后端倒掉清单缓存 + 图标磁盘浏览缓存，前端 bust 掉 webview 那层。
                      粒度取整份而非单张的理由见 `commands/rules.rs::rule_resources_refresh_icon_galleries`
                      的头注（两层必须一起作废 / 密排小格挂不下逐格按钮 / 它同时是浏览痕迹的清除入口）。
                      loading 时禁用：连点会叠出多次真拉取（每次都是三源九镜像）。 */}
                  <button
                    type="button"
                    className="btn ghost sm"
                    disabled={galleryStatus === 'loading'}
                    onClick={() => void loadGalleries(true)}
                  >
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                      <path d="M20 12a8 8 0 10-2.3 5.7" />
                      <path d="M20 6v6h-6" />
                    </svg>
                    <span>{t('common.refresh', '刷新')}</span>
                  </button>
                </div>
                <div className="aad-ico-grid" onScroll={onGridScroll}>
                  {galleryStatus === 'idle' || galleryStatus === 'loading' ? (
                    // idle 只存在于「面板刚展开、effect 还没跑」的那一帧，对用户就是加载中。
                    <div className="aad-hint">{t('common.loading', '加载中…')}</div>
                  ) : galleryStatus === 'error' ? (
                    // 加载失败态：可重试（真拉取会瞬时失败——网络/CDN 抖动），或降级改用 URL 面板。
                    // 与「真的空」区分：失败不是「没图标」，重试通常能恢复。
                    <div className="aad-hint">
                      <div>
                        {t('appAdd.galleryError', '图标库加载失败，可重试或改用「URL」手动填写图标地址')}
                      </div>
                      <div style={{ display: 'flex', gap: 8, marginTop: 8 }}>
                        <button
                          type="button"
                          className="btn ghost sm"
                          onClick={() => void loadGalleries()}
                        >
                          {t('appAdd.galleryRetry', '重试')}
                        </button>
                        <button
                          type="button"
                          className="btn ghost sm"
                          onClick={() => setIconMethod('url')}
                        >
                          {t('appAdd.galleryUseUrl', '改用 URL 手动填写')}
                        </button>
                      </div>
                    </div>
                  ) : galleries.length === 0 ? (
                    // 真的空（两源都拉到但无 icons，罕见）：纯引导改用 URL 面板（用户明确要求的降级路径），
                    // 不新起弹窗/抽象，直接复用 iconMethod 状态机已有的 'url' 分支。
                    <div className="aad-hint">
                      <div>
                        {t('appAdd.galleryEmpty', '图标库暂不可用，可改用「URL」手动填写图标地址')}
                      </div>
                      <button
                        type="button"
                        className="btn ghost sm"
                        style={{ marginTop: 8 }}
                        onClick={() => setIconMethod('url')}
                      >
                        {t('appAdd.galleryUseUrl', '改用 URL 手动填写')}
                      </button>
                    </div>
                  ) : filteredGalleries.length === 0 ? (
                    <div className="aad-hint">{t('appAdd.noMatchIcon', '无匹配图标')}</div>
                  ) : (
                    <>
                      {shownGalleries.map((g) => (
                        <button
                          key={g.url}
                          type="button"
                          className={cn('aad-ico-cell', onlineSel?.url === g.url && 'on')}
                          data-tip={g.name}
                          onClick={() => setOnlineSel(g)}
                        >
                          {/* 三态标记（`data-ico` 缺省 / ok / err）：原先失败只是把 img 隐掉，剩下
                              白瓷砖底色（`.aad-ico-cell` 的 `background:#fff`）——于是「请求压根没发」
                              与「发了但失败」在屏幕上是同一副面孔：白方块。真机排查一整轮卡在这个
                              不可分上。标记直接写 DOM 属性而不进 React state：逐格 setState 会把
                              一次画廊渲染放大成一批 setState 提交。
                              **无 `loading="lazy"`**：它正是真机白块的元凶（见 GALLERY_PAGE 头注），
                              并发改由分批渲染约束。 */}
                          <img
                            src={`${iconProxySrc(g.url)}${bustSuffix}`}
                            alt=""
                            onLoad={(e) => {
                              e.currentTarget.parentElement?.setAttribute('data-ico', 'ok');
                            }}
                            onError={(e) => {
                              (e.currentTarget as HTMLElement).style.display = 'none';
                              e.currentTarget.parentElement?.setAttribute('data-ico', 'err');
                            }}
                          />
                        </button>
                      ))}
                      {filteredGalleries.length > shownGalleries.length && (
                        <div className="aad-hint" style={{ gridColumn: '1 / -1' }}>
                          {t('appAdd.galleryMore', {
                            defaultValue: '已显示 {{shown}} / {{total}}，继续滚动加载更多',
                            shown: shownGalleries.length,
                            total: filteredGalleries.length,
                          })}
                        </div>
                      )}
                    </>
                  )}
                </div>
              </div>
            )}

            {iconMethod === 'url' && (
              <input
                className="input mono"
                style={{ marginTop: 8 }}
                value={iconUrlInput}
                onChange={(e) => setIconUrlInput(e.target.value)}
                placeholder="https://…/icon.png"
              />
            )}

            {iconMethod === 'emoji' && (
              <div className="emoji-pal">
                {EMOJI_PALETTE.map((e) => (
                  <button
                    key={e}
                    type="button"
                    className={cn('emoji-opt', emoji === e && 'on')}
                    onClick={() => setEmoji(e)}
                  >
                    {e}
                  </button>
                ))}
              </div>
            )}

            {iconMethod === 'letter' && (
              <div className="aad-hint">
                {t('appAdd.letterHint', '以应用名首字母作图标；可切换「在线图标」浏览图标库')}
              </div>
            )}
          </div>
        </div>
      </div>

      {/* 名称 + 分类 */}
      <div className="fld">
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 160px', gap: 10 }}>
          <div>
            <label className="fld-l" htmlFor="aad-name">
              <span>{t('appAdd.name', '名称')}</span> <span className="req-star">*</span>
            </label>
            <input
              id="aad-name"
              className="input"
              value={name}
              onChange={(e) => {
                setName(e.target.value);
                setErrName(false);
              }}
              placeholder={t('appAdd.namePh', '应用名称')}
            />
          </div>
          <div>
            <label className="fld-l" htmlFor="aad-cat">
              {t('appAdd.category', '分类')}
            </label>
            <Csel
              id="aad-cat"
              ariaLabel={t('appAdd.category', '分类')}
              value={category}
              onChange={setCategory}
              options={categoryOptions}
            />
          </div>
        </div>
        {errName && <div className="err-line">{t('appAdd.errName', '请填写应用名称')}</div>}
        {category === 'custom' && (
          <input
            className="input"
            style={{ marginTop: 8 }}
            value={customCategory}
            onChange={(e) => setCustomCategory(e.target.value)}
            placeholder={t('appAdd.catCustomPh', '自定义分类名')}
          />
        )}
      </div>

      {/* 资源标签 */}
      <div className="fld">
        <label className="fld-l">
          <span>{t('appAdd.resTags', '资源标签')}</span> <span className="req-star">*</span>{' '}
          <span className="fld-opt">{t('appAdd.resTagsHint', 'Geosite 必选 · GeoIP 可选')}</span>
        </label>
        <div className="aad-res">
          <div className="aad-res-bar">
            <div className="seg2" role="group" aria-label="Tag kind" style={{ display: 'flex' }}>
              <button
                type="button"
                style={{ flex: 1 }}
                className={cn(tagKind === 'geosite' && 'on')}
                onClick={() => setTagKind('geosite')}
              >
                Geosite
              </button>
              <button
                type="button"
                style={{ flex: 1 }}
                className={cn(tagKind === 'geoip' && 'on')}
                onClick={() => setTagKind('geoip')}
              >
                GeoIP
              </button>
            </div>
            <label className="input search-box">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} style={{ width: 14 }}>
                <circle cx="11" cy="11" r="7" />
                <path d="M20 20l-3-3" />
              </svg>
              <input
                value={tagQuery}
                onChange={(e) => setTagQuery(e.target.value)}
                placeholder={t('appAdd.tagSearchPh', '搜索标签…')}
              />
            </label>
          </div>
          <div className="tag-pick">
            {filteredTags.length === 0 ? (
              <div className="aad-hint">{t('appAdd.noMatchTag', '无匹配标签')}</div>
            ) : (
              filteredTags.map((tg) => (
                <button
                  key={tg}
                  type="button"
                  className={cn('tagchip', curSel.has(tg) && 'on')}
                  onClick={() => toggleTag(tagKind, tg)}
                >
                  {tg}
                </button>
              ))
            )}
          </div>
          {selectedChips.length > 0 && (
            <div className="aad-tag-sel">
              {selectedChips.map((c) => (
                <span
                  key={`${c.kind}:${c.tag}`}
                  className={cn('aad-sel-chip', !available.has(`${c.kind}-${c.tag}`) && 'miss')}
                  role="button"
                  tabIndex={0}
                  onClick={() => toggleTag(c.kind, c.tag)}
                >
                  <span className="aad-sel-k">{c.kind === 'geoip' ? 'IP' : 'SITE'}</span>
                  {c.tag}
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.2}>
                    <path d="M5 5l14 14M19 5L5 19" />
                  </svg>
                </span>
              ))}
            </div>
          )}
          {missingChips.length > 0 && (
            <div className="warn-line">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M12 9v4M12 17h.01" />
                <path d="M10.3 3.9 1.8 18a2 2 0 001.7 3h17a2 2 0 001.7-3L13.7 3.9a2 2 0 00-3.4 0z" />
              </svg>
              <span>
                {t('appAdd.missingHint', {
                  defaultValue: '{{n}} 个资源标签尚未下载到本地',
                  n: missingChips.length,
                })}
              </span>
              <button
                type="button"
                className="btn ghost sm"
                onClick={() => {
                  navigate('resources');
                  closeAll();
                }}
              >
                {t('appAdd.gotoResources', '前往规则资源')}
              </button>
            </div>
          )}
          {errGeosite && <div className="err-line">{t('appAdd.errGeosite', '请至少选择一个 Geosite 标签')}</div>}
        </div>
      </div>

      {/* 进程名 */}
      <div className="fld">
        <label className="fld-l" htmlFor="aad-proc">
          {t('appAdd.procNames', '进程名')}
        </label>
        <input
          id="aad-proc"
          className="input mono"
          value={proc}
          onChange={(e) => setProc(e.target.value)}
          placeholder="chrome.exe, slack"
        />
        <button
          type="button"
          className="btn ghost sm"
          style={{ marginTop: 8 }}
          onClick={() => open({ kind: 'proc-pick', onPick: handleProcPick })}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <rect x="4" y="4" width="16" height="16" rx="2" />
            <path d="M9 9h6v6" />
          </svg>
          <span>{t('appAdd.procPick', '从进程选择')}</span>
        </button>
      </div>
    </Modal>
  );
}

export default AppAddDialog;
