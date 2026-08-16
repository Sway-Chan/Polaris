/**
 * 连接拓扑卡（首页 Sankey）——原型 polaris-prototype.html:1700-1708 + 3505-3538(renderSankey) + 3773-3796(菜单/tooltip)。
 *
 * 结构：.card.pad.topo > .topo-head（标题 + hint + 检索框）+ .sankey-wrap（SVG + tooltip + 右键菜单 + 窄容器 fallback）。
 * 布局纯函数在 topology-layout.ts（几何取原型、缩放取 issue #303 定稿，理由见该文件头）。
 *
 * 三处边界约束（issue #303 题 2 实证：原型无边界处理，真机下 tooltip/菜单溢出卡片）：
 *  - 右键菜单 clamp 在 .sankey-wrap 内（原型 showCtx:3770-3771 同款 Math.max(8, Math.min(...)) 语义）；
 *  - tooltip 同样 clamp + 近边翻转 —— 故不用原型 .sk-tip 的 transform:translate(-50%,-8px)（它无边界感知）；
 *  - 命中区经 hitBox 与视觉尺寸解耦（条细到 2px 仍可点/可右键）。
 * .card 无 overflow:hidden（components.css:42），故 absolute 挂 wrap 即可，无需 Portal。
 *
 * 高亮：hover 与检索共用 collectLinkedIds 的 BFS 焦点集（上游-topology-search.md 选 A）。hover 临时优先，
 * 指针离开恢复检索高亮。非空检索由后端在完整活动表上先过滤再聚合，常态 Top-15 只是绘制投影，
 * 不再充当检索数据源。
 */
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute } from '@/lib/staged-config';
import { validateRuleValue, type RuleSubject } from '@/domain/rules';
import { RuleSubjectMenuItems } from '@/components/rule-subject-menu';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { cn } from '@/lib/utils';
import { clampToWrap } from '@/lib/overlay-position';
import { TOPOLOGY_OTHERS_KEY, type ConnectionsAggregate } from '@/contracts/types';
import {
  collectLinkedIds,
  computeTopologyLayout,
  hitBox,
  isRuleableHost,
  matchNodeIds,
  NODE_WIDTH,
  type TopoLink,
  type TopoNode,
} from './topology-layout';

const TIP_OFFSET = 12; // tooltip 相对指针偏移
const SEARCH_DEBOUNCE_MS = 180;

interface SearchProjection {
  query: string;
  aggregate: ConnectionsAggregate;
}

interface ContextMenuState {
  x: number; // 相对 .sankey-wrap
  y: number;
  value: string;
}

interface ConnectionTopologyProps {
  aggregate: ConnectionsAggregate | null;
  /** 代理未运行 —— 断开态渲染原型的 stub 空态（见下方 `#sankey-stub`），而非「零连接」那行灰字。 */
  disconnected: boolean;
}

function ConnectionTopologyView({ aggregate, disconnected }: ConnectionTopologyProps) {
  const { t } = useTranslation();
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);

  const wrapRef = useRef<HTMLDivElement>(null);
  const svgRef = useRef<SVGSVGElement>(null);
  const tipRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const [size, setSize] = useState({ w: 0, h: 0 });
  const [search, setSearch] = useState('');
  const [searchProjection, setSearchProjection] = useState<SearchProjection | null>(null);
  const [hovered, setHovered] = useState<{ kind: 'node' | 'link'; id: string } | null>(null);
  const [tip, setTip] = useState<{ text: string; clientX: number; clientY: number } | null>(null);
  const [tipSize, setTipSize] = useState({ w: 0, h: 0 });
  const [menu, setMenu] = useState<ContextMenuState | null>(null);
  const [menuSize, setMenuSize] = useState({ w: 0, h: 0 });

  const normalizedSearch = search.trim().toLowerCase();
  const searching = normalizedSearch !== '';
  const searchResolved = searchProjection?.query === normalizedSearch;

  /* 非空检索：先等完整表变更监听真登记，再发首个查询；之后每个 250ms 合并后的变更信号重查。
     请求世代守卫保证慢回包不能覆盖后发的新真值。常态不挂该监听、不增加第二份长驻索引。 */
  useEffect(() => {
    if (!searching || disconnected) {
      setSearchProjection(null);
      return;
    }
    let alive = true;
    let off: (() => void) | null = null;
    let timer: number | null = null;
    let requestSequence = 0;

    const refresh = () => {
      const sequence = ++requestSequence;
      void api.stats
        .searchTopology(normalizedSearch)
        .then((next) => {
          if (alive && sequence === requestSequence) {
            setSearchProjection({ query: normalizedSearch, aggregate: next });
          }
        })
        .catch(() => {
          /* IPC 收口层已记录；保留上一份已核实投影，不编造空结果。 */
        });
    };
    const schedule = (delay: number) => {
      if (timer !== null) window.clearTimeout(timer);
      timer = window.setTimeout(refresh, delay);
    };

    void api.stats
      .onConnectionsTopologyChangedReady(() => schedule(0))
      .then((unlisten) => {
        if (!alive) {
          unlisten();
          return;
        }
        off = unlisten;
        if (timer === null) schedule(SEARCH_DEBOUNCE_MS);
      })
      .catch(() => {
        // 监听故障时至少给出一次完整表快照；不能退回用 Top-15 断言“无结果”。
        if (alive) schedule(SEARCH_DEBOUNCE_MS);
      });

    return () => {
      alive = false;
      requestSequence += 1;
      if (timer !== null) window.clearTimeout(timer);
      off?.();
    };
  }, [disconnected, normalizedSearch, searching]);

  const displayAggregate = searching && searchResolved
    ? searchProjection.aggregate
    : aggregate;

  /* SVG 实测尺寸：.sankey 有 contain:size（防 viewBox 比例把旧高度反馈回 flex 链造成「高度棘轮」），
     故 intrinsic 尺寸不可信，必须走 getBoundingClientRect + ResizeObserver。
     依赖必须带 `disconnected`：SVG 只在连通态渲染，断开态挂载时 ref 为 null、effect 空跑一次就再不重来，
     于是连上后 size 恒 0 ⇒ 布局算出零节点 ⇒ 「启动后要切一次导航拓扑才出来」（切导航 = 重挂载 = effect 重跑）。 */
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    const measure = () => {
      const r = svg.getBoundingClientRect();
      setSize({ w: Math.round(r.width), h: Math.round(r.height) });
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(svg);
    return () => ro.disconnect();
  }, [disconnected]);

  const { nodes, links } = useMemo(
    () =>
      displayAggregate
        ? computeTopologyLayout(displayAggregate, size.w, t, size.h)
        : { nodes: [] as TopoNode[], links: [] as TopoLink[] },
    [displayAggregate, size.w, size.h, t]
  );

  const searchMatches = useMemo(() => matchNodeIds(nodes, search), [nodes, search]);

  /* 焦点集：hover 临时优先于检索（指针离开自动恢复检索高亮）。null = 未激活（全图正常）。
     检索零命中 → 空焦点集 → collectLinkedIds 返回空集 → 全图淡出，与「未激活」区分开。 */
  const highlighted = useMemo(() => {
    let focus: string[] | null = null;
    if (hovered) {
      if (hovered.kind === 'node') focus = [hovered.id];
      else {
        const link = links.find((l) => l.id === hovered.id);
        focus = link ? [link.source, link.target] : [];
      }
    } else if (searching) {
      focus = searchMatches;
    }
    return focus === null ? null : collectLinkedIds(links, focus);
  }, [hovered, searching, searchMatches, links]);

  const tipPos = useMemo(() => {
    const wrap = wrapRef.current?.getBoundingClientRect();
    if (!tip || !wrap) return null;
    return clampToWrap(wrap, tip.clientX, tip.clientY, tipSize, TIP_OFFSET);
  }, [tip, tipSize]);

  const menuPos = useMemo(() => {
    const wrap = wrapRef.current?.getBoundingClientRect();
    if (!menu || !wrap) return null;
    return clampToWrap(wrap, menu.x + wrap.left, menu.y + wrap.top, menuSize, 0);
  }, [menu, menuSize]);

  /* 浮层尺寸随内容变（域名长短/菜单文案），每次换目标后重测。 */
  useLayoutEffect(() => {
    if (!tip || !tipRef.current) return;
    const r = tipRef.current.getBoundingClientRect();
    setTipSize((p) => (p.w === r.width && p.h === r.height ? p : { w: r.width, h: r.height }));
  }, [tip]);

  useLayoutEffect(() => {
    if (!menu || !menuRef.current) return;
    const r = menuRef.current.getBoundingClientRect();
    setMenuSize((p) => (p.w === r.width && p.h === r.height ? p : { w: r.width, h: r.height }));
  }, [menu]);

  /* 点击空白 / ESC 关菜单（原型 3781-3786 的 document click 收敛为组件内自管）。 */
  useEffect(() => {
    if (!menu) return;
    const onDown = (e: MouseEvent) => {
      if (!menuRef.current?.contains(e.target as Node)) setMenu(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenu(null);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [menu]);

  const openMenu = useCallback((value: string, clientX: number, clientY: number) => {
    const wrap = wrapRef.current?.getBoundingClientRect();
    if (!wrap) return;
    setMenu({ x: clientX - wrap.left, y: clientY - wrap.top, value });
  }, []);

  const menuSubject = useMemo<RuleSubject | null>(() => {
    if (!menu) return null;
    if (validateRuleValue('ipCidr', menu.value)) {
      return { kind: 'ip', type: 'ipCidr', value: menu.value };
    }
    if (validateRuleValue('domain', menu.value)) {
      return { kind: 'domain', type: 'domain', value: menu.value };
    }
    return null;
  }, [menu]);

  /**
   * 快速代理/直连：类型由当前观测对象决定，与“新建规则”弹窗和“加入已有”共用同一对象。
   *
   * `remarks` 必填而非可选：规则列表的标题在无 remarks 时回落成**裸类型名**
   * （`RuleItem.tsx::ruleTitle`），同类型快速规则会完全无法区分，而顺序又直接决定命中优先级。
   * 弹窗腿早已把 remarks 设成必填
   * （`RuleDialog::handleSubmit`），同一个入口不该两套要求。
   */
  const addSubjectRule = useCallback(
    async (subject: RuleSubject, action: 'proxy' | 'direct') => {
      setMenu(null);
      const actionLabel = action === 'proxy' ? t('home.ruleProxy') : t('home.ruleDirect');
      try {
        const rule = {
          type: subject.type,
          values: [subject.value],
          action,
          enabled: true,
          remarks: t('home.ruleRemarks', {
            action: actionLabel,
            value: subject.value,
          }),
        };
        // 配置暂存闸门（与 NodeDialog 同形）：`customRules` Class B，无副作用 ⇒ 默认腿。
        // 新增时前端自铸 id（后端只在落盘那一刻发 id，而条目现在就要一个稳定的实体寻址键）。
        if (editRoute('customRules', stagingEnabled) === 'staged') {
          const entityId = crypto.randomUUID();
          stage({
            id: `rule:${entityId}`,
            kind: 'rule',
            label: `${t('rules.newTitle')} ${subject.value}`,
            entityPath: ['customRules', entityId],
            nextValue: { ...rule, id: entityId },
          });
        } else {
          await api.rules.add(rule);
        }
        toast.success(t('home.ruleAdded', { value: subject.value, action: actionLabel }));
      } catch {
        toast.error(t('home.ruleAddFail'));
      }
    },
    [t, stagingEnabled, stage]
  );

  const hosts = displayAggregate?.hosts ?? [];
  const maxHost = hosts.reduce((m, h) => Math.max(m, h.count), 0) || 1;
  const hostLabel = (n: string) => (n === TOPOLOGY_OTHERS_KEY ? t('home.others') : n);

  const dim = highlighted !== null;
  const isHl = (id: string) => highlighted?.has(id) ?? false;

  return (
    <div className={cn('card pad topo', disconnected && 'disconnected')} id="topo-card">
      <div className="topo-head">
        <h3 data-tip={t('home.topologyHint')}>{t('home.connectionFlow')}</h3>
        <span className="tw">
          {t('home.connections')}: {displayAggregate?.total ?? 0}
        </span>
        {/* 检索框：复用 .input + .search-box 通用皮肤（icon + borderless input + focus ring），.topo-search 仅覆盖尺寸。
            .topo-search 必须定义在 .input 之后 —— 同特异性下 .input 的 width:100% 会反压把卡头挤成竖排（上游 真机实证）。 */}
        <label className="input search-box topo-search">
          <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
            <circle cx="11" cy="11" r="7" />
            <path d="M20 20l-3.5-3.5" />
          </svg>
          <input
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t('home.searchTopology')}
            aria-label={t('home.searchTopology')}
          />
          {search && (
            <button type="button" className="search-clear" onClick={() => setSearch('')} aria-label={t('home.clear')}>
              <svg viewBox="0 0 24 24" width="13" fill="none" stroke="currentColor" strokeWidth="2">
                <path d="M6 6l12 12M18 6L6 18" />
              </svg>
            </button>
          )}
        </label>
      </div>

      <div className="sankey-wrap" id="sankey-wrap" ref={wrapRef}>
        {/* 断开态**不渲染图表本体** —— 原型 `showSankeyStub` 的语义是 stub **取代**图表，不是并列。
            此前只在下面追加 stub 而 SVG 照常渲染：`.sankey` 有 `contain:size` + 撑满容器高，
            于是 stub 被顶到图表下方（真机现象「代理未运行漂移到页面下方」，陈先生 2026-07-29 报）。
            这里用条件渲染而非 CSS 隐藏：`.disconnected` 两侧都无皮肤（原型亦然），加一条只为隐藏的
            规则等于在禁区文件外再造一份状态样式；且断开态本就不需要跑布局计算。 */}
        {!disconnected && (
        <svg
          ref={svgRef}
          className={cn('sankey', dim && 'dim')}
          viewBox={`0 0 ${size.w} ${size.h}`}
          role="img"
          aria-label={t('home.connectionFlow')}
          onMouseLeave={() => {
            setHovered(null);
            setTip(null);
          }}
        >
          <defs>
            {/* 原型 :1529 linkGrad：三列共用单一渐变（非 上游的 source/rule 两套） */}
            <linearGradient id="linkGrad" x1="0" y1="0" x2="1" y2="0">
              <stop offset="0" stopColor="hsl(var(--aurora))" stopOpacity="0.55" />
              <stop offset="1" stopColor="hsl(var(--flow))" stopOpacity="0.55" />
            </linearGradient>
          </defs>

          {nodes.length > 0 && (
            <>
              <text className="col-lbl" x={size.w * 0.12} y={16} textAnchor="middle">
                {t('home.topoColDevice')}
              </text>
              <text className="col-lbl" x={size.w * 0.5} y={16} textAnchor="middle">
                {t('home.topoColDomain')}
              </text>
              <text className="col-lbl" x={size.w * 0.88} y={16} textAnchor="middle">
                {t('home.topoColOutbound')}
              </text>
            </>
          )}

          {links.map((l) => (
            <path
              key={l.id}
              className={cn('link', isHl(l.id) && 'hl')}
              d={l.path}
              onMouseEnter={() => setHovered({ kind: 'link', id: l.id })}
              onMouseMove={(e) =>
                setTip({
                  text: `${nodeName(nodes, l.source)} → ${nodeName(nodes, l.target)} · ${l.value}`,
                  clientX: e.clientX,
                  clientY: e.clientY,
                })
              }
              onMouseLeave={() => setTip(null)}
            />
          ))}

          {nodes.map((n) => {
            const hb = hitBox(n);
            // ruleable = 可作「加规则」目标（host 且非「其它」聚合、名字像域名/IP）。
            // 「其它」sentinel 与回落成路由规则名的 host 只保留 hover 高亮，不开右键菜单——为它们加 domainSuffix 规则会写垃圾。
            const ruleable = isRuleableHost(n);
            const labelLeft = n.type !== 'outbound';
            return (
              <g
                key={n.id}
                className={cn('node', ruleable && 'dnode', isHl(n.id) && 'hl')}
                transform={`translate(${n.x}, ${n.y})`}
                {...(ruleable
                  ? {
                      tabIndex: 0,
                      role: 'button',
                      'aria-label': n.name,
                      onKeyDown: (e: React.KeyboardEvent<SVGGElement>) => {
                        if (e.key !== 'Enter' && e.key !== ' ') return;
                        e.preventDefault();
                        const r = e.currentTarget.getBoundingClientRect();
                        openMenu(n.name, r.right, r.top);
                      },
                    }
                  : {})}
              >
                <rect width={NODE_WIDTH} height={n.height} fill={n.color} />
                <text
                  className="nlabel"
                  x={labelLeft ? -7 : NODE_WIDTH + 7}
                  y={n.height / 2 + 4}
                  textAnchor={labelLeft ? 'end' : 'start'}
                  pointerEvents="none"
                >
                  {n.name}
                </text>
                {/* 命中区：透明矩形，尺寸与视觉条解耦（hitBox），覆盖标签侧——标签 pointer-events:none 会穿透 */}
                <rect
                  x={hb.x}
                  y={hb.y}
                  width={hb.width}
                  height={hb.height}
                  fill="transparent"
                  onMouseEnter={() => setHovered({ kind: 'node', id: n.id })}
                  onMouseMove={(e) =>
                    setTip({ text: `${n.name} · ${n.value}`, clientX: e.clientX, clientY: e.clientY })
                  }
                  onMouseLeave={() => setTip(null)}
                  onContextMenu={
                    ruleable
                      ? (e) => {
                          e.preventDefault();
                          openMenu(n.name, e.clientX, e.clientY);
                        }
                      : undefined
                  }
                  onClick={ruleable ? (e) => openMenu(n.name, e.clientX, e.clientY) : undefined}
                />
              </g>
            );
          })}
        </svg>
        )}

        {/* tooltip：JS 定位 + clamp/翻转（原型 .sk-tip 的 transform 无边界感知，issue303 题2② 实证会溢出卡片） */}
        {tip && (
          <div ref={tipRef} className="sk-tip show" style={tipPos ?? { left: 0, top: 0, opacity: 0 }}>
            {tip.text}
          </div>
        )}

        {/* 只有完整表查询已经回包且总数为零，才能断言“无命中”；等待期不拿 Top-15 猜。 */}
        {searching && searchResolved && displayAggregate?.total === 0 && (
          <div className="sk-nomatch">{t('home.searchTopologyNoMatch')}</div>
        )}

        {/* 零连接空态。此前**只有窄容器有**：空文案写在 `.sankey-fallback` 里，而该块被
            `@container topo (max-width:540px)` 锁死（prototype.css:522），宽屏下 `display:none`
            ⇒ 拓扑是首页视觉主体，零连接时整块纯白，与「加载中/坏了」无从区分。提到 SVG 同级，
            并由 `.sankey-empty` 在窄容器下隐藏，避免与 fallback 那句重复。 */}
        {nodes.length === 0 && !disconnected && !searching && (
          <div className="sankey-empty">{t('home.noActiveConnections')}</div>
        )}

        {/* 断开态 stub（原型 `showSankeyStub`，`proto:3417-3423`）：icon + 标题 + 说明。
            三段皮肤 `.stub` / `.stub-ic` / `h4` / `p` 早在 `styles/prototype.css:1363-1366`，
            此前零消费方 ⇒ 首启第一屏只剩一行灰字。断开态与「连着但零连接」是两回事：
            前者说明代理没跑，后者说明跑着但没流量，故两个空态互斥而非叠加。
            **不带 CTA**（原型有 `data-act="connect"`）：首页顶部圆钮就是同一个动作，同屏两颗主 CTA
            是重复入口（陈先生 2026-07-29 两轮点名）。尺寸对齐见 index.css `#sankey-stub`。 */}
        {disconnected && (
          <div className="stub" id="sankey-stub">
            <svg className="stub-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M3 12h4l3 7 4-14 3 7h4" />
            </svg>
            <h4>{t('home.stubProxyStopped')}</h4>
            <p>{t('home.stubProxyStoppedDesc')}</p>
          </div>
        )}

        {/* 右键菜单：原型 .ctx-menu/.ctx-i（:1294-1300）+ showCtx 的容器内 clamp（:3770-3771） */}
        {menu && menuSubject && (
          <div ref={menuRef} className="ctx-menu" style={menuPos ?? { left: 0, top: 0, opacity: 0 }}>
            {/* 「加入已有规则…」+「加入自定义规则」两项与连接页共用同一个组件（含排序判据与写入腿）。 */}
            <RuleSubjectMenuItems subject={menuSubject} onDone={() => setMenu(null)} />
            <button type="button" className="ctx-i" onClick={() => addSubjectRule(menuSubject, 'proxy')}>
              <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M5 12l5 5 9-11" />
              </svg>
              {t('home.ruleProxy')}
            </button>
            <button type="button" className="ctx-i" onClick={() => addSubjectRule(menuSubject, 'direct')}>
              <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M4 4l16 16M4 20L20 4" />
              </svg>
              {t('home.ruleDirect')}
            </button>
          </div>
        )}

        {/* 窄容器兜底（@container topo max-width:540px 时 .sankey 隐藏）：原型 renderSankeyFallback:3534-3540 单列条形。
            **条件渲染而非常驻**：Top-15 满载帧下这块是 81 个 DOM 节点，占 `#topo-card` 全部 225 个后代的 36%
            （Chromium/WebKit 双引擎实测），而宽屏下它自始至终 `display:none` —— 纯白付，且随 250ms 节拍 ×4。

            判据取 `size.w === 0` 而**不新引 ResizeObserver**：`.sankey` 被 container query 置 `display:none` 时，
            上面那个既有的 RO（:76-87）会回报 0×0（Chromium/WebKit 双引擎实测，`afterHide=[[0,0]]`）⇒
            「SVG 量不到尺寸」与「图表被 CSS 隐藏」同真值。故这里不是新增观测，是把既有观测多用一次，
            零新增 observer / 零新增重排监听 —— 否则「省 80 个节点、换一个 observer」未必是净收益。
            语义上也自洽且与断点解耦：**SVG 量不到尺寸就画不出图，那就该出兜底列表**，本文件不必知道 540px 在哪。

            叠 `!disconnected`：断开态由 `#sankey-stub` **取代**整块（同上 :249 的「stub 取代而非并列」），
            兜底列表与 stub 同屏是两个重复空态。

            已知代价：挂载首帧 size 还是 0（RO 在 effect 里才首测），故宽屏下这 81 个节点会被建一次再拆掉。
            那是每次进首页一次，不是每帧一次 —— 与要省掉的 4Hz×81 不同量级；且宽屏下它恒 `display:none`，
            用户看不到闪动。 */}
        {!disconnected && size.w === 0 && (
        <div className="sankey-fallback" id="sankey-fallback">
          {hosts.length === 0 ? (
            <div className="card-sub">{t('home.noActiveConnections')}</div>
          ) : (
            hosts.map((h) => (
              <div className="top-bar-row" key={h.name}>
                <span className="tb-name" data-tip={hostLabel(h.name)}>
                  {hostLabel(h.name)}
                </span>
                <span className="bar">
                  <i style={{ width: `${(h.count / maxHost) * 100}%`, background: 'hsl(var(--aurora))' }} />
                </span>
                <span className="tb-v">{h.count}</span>
              </div>
            ))
          )}
        </div>
        )}
      </div>
    </div>
  );
}

function nodeName(nodes: TopoNode[], id: string): string {
  return nodes.find((n) => n.id === id)?.name ?? id;
}

/**
 * `memo` 是本组件的**必需接线**，不是可选优化。
 *
 * 宿主 `HomeScreen` 是个 1200+ 行的胖屏，自持十余个与拓扑无关的 state（uptime 每秒 tick、启停 busy、
 * 测速中、解锁冷却、出口选单开合…）外加多个 zustand 订阅与系统代理活态轮询 —— 其中任何一个变一下，
 * 无 memo 时整棵 Sankey（Top-15 满载帧 = 20 节点 + 47 缎带）就要重建一遍 VDOM 并整树 diff。
 * 拓扑节拍从 1s 提到 250ms（`AGGREGATE_POLL_INTERVAL`）后，帧本身的重渲已经是原来的 4 倍，
 * 再叠上这些白付的重渲就是 WebKit 侧 `Graphics and Media` 那 73MB / GPU 2.5% 的一部分来源。
 *
 * 生效前提是**两个 props 都引用稳定**，这不是巧合而是要保持的不变式：
 *  - `aggregate`：`HomeScreen` 的 `useState`，只在收到新 aggregate 帧时换引用（后端已按内容签名去重）；
 *  - `disconnected`：`!connected` 布尔量。
 * 二者都不在渲染期新建对象/闭包，故默认浅比较即可，无需 `useMemo`/`useCallback` 包装。
 * 调用点若哪天塞进内联对象或箭头函数，memo 会恒失效、反而多一层比较 —— 该不变式由
 * `topology-render-budget.test.ts` 钉住。
 *
 * 本机实测（Playwright Chromium 149 + WebKit 26.5 双引擎，量测装置见提交说明）：无关父态变化 60 次，
 * 加 memo 前该子树渲染 60 次，加后 0 次。
 */
export const ConnectionTopology = memo(ConnectionTopologyView);
