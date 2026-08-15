/**
 * Connections 屏（逐元素复现原型 polaris-prototype.html #s-connections L2000-2040 +
 * 动态行模板 renderConn/renderTopN L5037-5047/3765-3771，rebuild-plan B3）。
 *
 * 结构对齐原型：
 *  - .phead（标题）
 *  - .conn-toolbar（拓扑/活动/已结束 + 当前列表工具）
 *  - #conn-table-view（.conn-scroll > .conn-list-wrap > table.conn-table：域名/目标/规则/出站链/上下行/累计/时长 + 关闭列，横向滚动）
 *  - #conn-top-view（Top-N 域名 + 出站分布，.top-grid）
 *
 * 功能接 api-client：
 *  - 明细：statsApi.subscribe('detail') + onConnectionsDetail（订阅驱动，进页订/离开退；
 *    后端 relay 消费 sing-box 连接流，按1s 合并下发 reset 基线 / 活动增量）
 *  - 已结束：订阅首帧/reset 为最多 1000 条全量，常态只合并本批新增/淘汰项
 *  - TOP：statsApi.subscribe('aggregate') + onConnectionsAggregate
 *  - 关单条：connectionsApi.close(id)（真调管理 API gRPC CloseConnection）+ 乐观移除（失败回滚）
 *  - 关全部：connectionsApi.closeAll()（真调 CloseAllConnections）
 *  - 暂停：**退订**冻结（不是只冻渲染）——暂停即 unsubscribe('detail')，后端据订阅集降 worker demand，
 *    整条 1s 轮询 + 逐帧序列化链路停机；恢复即重订，下一帧（≤1s）回填。
 *    **切到其它视图是同一次退订**：活动列表不再消费明细帧时，整条数据链立即停机。
 *    故工具栏里只作用于明细表的三个控件（搜索 / 暂停 / 关闭全部）在拓扑视图下一并隐掉，
 *    判据见 `.conn-toolbar` 处注释。
 *  - 排序：全部 9 个数据列本地可排序（rate/total 需前帧 diff 算速率）
 *  - 分页：搜索与排序始终覆盖全量数据，每页最多挂载 50 行；切走即卸载整张表 DOM
 */

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { RuleSubject } from '@/domain/rules';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { useAppStore } from '@/store/app-store';
import { RuleSubjectMenuItems } from '@/components/rule-subject-menu';
import { clampToWrap } from '@/lib/overlay-position';
import { createTopicSubscription } from '@/lib/topic-subscription';
import { useConfirmTwice } from '@/lib/confirm-twice';
import type {
  ClosedConnectionEntry,
  ConnectionEntry,
  ConnectionsDetailUpdate,
  ConnectionsAggregate,
  ConnectionsClosedUpdate,
} from '@/contracts/types';
import { TOPOLOGY_OTHERS_KEY } from '@/contracts/types';
import { fmtBytes, fmtDuration, fmtRate } from '../shared/format';
import {
  applyActiveDetailUpdate,
  clearActiveDetailState,
  type ActiveDetailSync,
} from './active-detail';
import { applyClosedHistoryUpdate } from './closed-history';
import { connectionRuleSubjects } from './connection-rule-subjects';

type ConnView = 'top' | 'active' | 'closed';

/** 本屏两个原地二次确认项（原型 :4113 `conn-close-all` / :4114 `conn-close-filtered`）。 */
const CLOSE_ALL_KEY = 'conn-close-all';
const CLOSE_FILTERED_KEY = 'conn-close-filtered';
const CLEAR_CLOSED_KEY = 'conn-clear-closed';
/**
 * 可排序列键 —— 表内**每个数据列**都在列（close 是操作列，不参与）。
 *
 * 契约要求 8 列可排序 = 上游 连接表的列集（type/source/dest/rule/chain/speed/traffic/time）。
 * Polaris 表把 上游的 source 列并进 host 列（域名 + sourceIP 副行）、另补了 Process 列 → 数据列共 9 个。
 * 9 个全可排序：契约那 8 个逐一到位，多出来的 Process 若单独留成不可排序，就是**新造**一处不一致。
 */
type SortKey =
  | 'type'
  | 'host'
  | 'dest'
  | 'rule'
  | 'chain'
  | 'rate'
  | 'total'
  | 'time'
  | 'ended'
  | 'proc';

/**
 * 每页上限。不用「超长占位行 + 虚拟滚动」：WebKit 会按整个 table 滚动面维持
 * graphics surface，即使 DOM 只有十几行，1000 条历史的占位高度仍可把图形驻留推到数百 MiB。
 * 50 行约为 5 个视口，连续浏览不至于过碎；搜索/排序仍先作用于全部 1000 条数据。
 */
const CONNECTION_PAGE_SIZE = 50;
/**
 * TOP 视图展示条数（原型 seg2 :2026，默认 10）。
 *
 * 上限对齐后端聚合真实能力：stats-engine `TOPOLOGY_TOP_N = 15`（types.rs:104）——
 * 后端只产出 Top-15 host + 1 个「其它」合并桶，20/50 永远填不满、纯误导，故砍到 5/10/15。
 */
const TOP_N_OPTIONS = [5, 10, 15] as const;
interface SortState {
  key: SortKey;
  dir: 1 | -1;
}

/** 连接行显示态（含本地派生：速率 / 累计 / 时长 / L4 类型 / 进程）。 */
interface ConnRow {
  entry: ConnectionEntry;
  host: string;
  dest: string;
  rule: string;
  chain: string;
  /** L4 类型 pill 文案（network 优先，回落 inbound type，缺则 —）。对齐 上游 typeOf。 */
  l4: string;
  /** L4 完整标签（network/type 拼，收进 `data-tip`）。 */
  l4Title: string;
  /** 是否 udp（pill 形态：tcp 实底 / udp 描边）。 */
  udp: boolean;
  /** 进程名（processPath basename）+ 完整路径（`data-tip`）。 */
  procName: string;
  procFull: string;
  /** 累计总字节（upload+download）。 */
  total: number;
  /** 上下行速率（bytes/s，与上一帧 diff / dt；首帧=0）。 */
  upRate: number;
  dnRate: number;
  /** 连接建立时刻 epoch ms；无有效时间为 NaN。 */
  startAt: number;
  /** 产生本行速率的 detail 序列；不是当前序列时速率按 0 展示。 */
  rateSequence: number;
  /** 已结束时间 epoch ms；活动连接为 null。 */
  endedAt: number | null;
}

/** 每秒会变的速率/时长不入缓存；只复用协议、目标、规则、链路和进程等静态派生字符串。 */
interface ConnStaticProjection {
  host: string;
  dest: string;
  rule: string;
  chain: string;
  l4: string;
  l4Title: string;
  udp: boolean;
  procName: string;
  procFull: string;
}

interface ConnStaticCacheEntry {
  host?: string;
  destinationIP?: string;
  destinationPort?: string;
  network?: string;
  inboundType?: string;
  processPath?: string;
  rule: string;
  rulePayload?: string;
  chain?: string;
  start?: string;
  startAt: number;
  projection: ConnStaticProjection;
}

function staticProjection(
  cache: Map<string, ConnStaticCacheEntry>,
  entry: ConnectionEntry,
): ConnStaticCacheEntry {
  const metadata = entry.metadata;
  const chain = entry.chains?.[0];
  const cached = cache.get(entry.id);
  if (
    cached !== undefined &&
    cached.host === metadata?.host &&
    cached.destinationIP === metadata?.destinationIP &&
    cached.destinationPort === metadata?.destinationPort &&
    cached.network === metadata?.network &&
    cached.inboundType === metadata?.type &&
    cached.processPath === metadata?.processPath &&
    cached.rule === entry.rule &&
    cached.rulePayload === entry.rulePayload &&
    cached.chain === chain &&
    cached.start === entry.start
  ) {
    return cached;
  }

  const destinationIP = metadata?.destinationIP;
  const network = metadata?.network ?? '';
  const procFull = metadata?.processPath ?? '';
  const l4Parts = [metadata?.network, metadata?.type].filter(Boolean);
  const projection: ConnStaticProjection = {
    host: metadata?.host || destinationIP || '—',
    dest: destinationIP
      ? `${destinationIP}${metadata?.destinationPort ? `:${metadata.destinationPort}` : ''}`
      : '—',
    rule: entry.rule
      ? `${entry.rule}${entry.rulePayload ? ` ${entry.rulePayload}` : ''}`
      : '—',
    chain: chain ?? '—',
    l4: metadata?.network || metadata?.type || '—',
    l4Title: l4Parts.length ? l4Parts.join('/') : '—',
    udp: network.toLowerCase() === 'udp',
    procName: procFull ? procFull.split(/[/\\]/).pop() || procFull : '—',
    procFull,
  };
  const result: ConnStaticCacheEntry = {
    host: metadata?.host,
    destinationIP,
    destinationPort: metadata?.destinationPort,
    network: metadata?.network,
    inboundType: metadata?.type,
    processPath: metadata?.processPath,
    rule: entry.rule,
    rulePayload: entry.rulePayload,
    chain,
    start: entry.start,
    startAt: entry.start ? Date.parse(entry.start) : Number.NaN,
    projection,
  };
  cache.set(entry.id, result);
  return result;
}

function projectConnection(
  entry: ConnectionEntry,
  endedAt: number | null,
  rates: { up: number; down: number },
  rateSequence: number,
  cache: Map<string, ConnStaticCacheEntry>,
): ConnRow {
  const upload = entry.upload ?? 0;
  const download = entry.download ?? 0;
  const { projection, startAt } = staticProjection(cache, entry);
  return {
    entry,
    ...projection,
    total: upload + download,
    upRate: rates.up,
    dnRate: rates.down,
    startAt,
    rateSequence,
    endedAt,
  };
}

function connectionAge(row: ConnRow, observedAt: number): number {
  return Number.isNaN(row.startAt)
    ? 0
    : Math.max(0, ((row.endedAt ?? observedAt) - row.startAt) / 1000);
}

function connectionRates(row: ConnRow, activeSequence: number): { up: number; down: number } {
  return row.endedAt === null && row.rateSequence === activeSequence
    ? { up: row.upRate, down: row.dnRate }
    : { up: 0, down: 0 };
}

export function ConnectionsScreen() {
  const { t } = useTranslation();
  const privacyMode = useAppStore((s) => s.privacyMode);

  const [view, setView] = useState<ConnView>('top');
  const [search, setSearch] = useState('');
  const [paused, setPaused] = useState(false);
  const [sort, setSort] = useState<SortState | null>(null);
  const [page, setPage] = useState(0);

  const [rows, setRows] = useState<ConnRow[]>([]);
  const [activeClock, setActiveClock] = useState({ at: 0, sequence: 0 });
  const [closedEntries, setClosedEntries] = useState<ClosedConnectionEntry[]>([]);
  const [activeLoaded, setActiveLoaded] = useState(false);
  const [closedLoaded, setClosedLoaded] = useState(false);
  const [aggregate, setAggregate] = useState<ConnectionsAggregate | null>(null);
  const [topN, setTopN] = useState<number>(10);
  const tableScrollRef = useRef<HTMLDivElement>(null);

  // 上一帧字节记账（算速率）：id → {up, dn, at(ms)}
  const prevRef = useRef<Map<string, { up: number; dn: number; at: number }>>(
    new Map()
  );
  const activeStaticRef = useRef<Map<string, ConnStaticCacheEntry>>(new Map());
  const activeIndexRef = useRef<Map<string, ConnectionEntry>>(new Map());
  const activeSyncRef = useRef<ActiveDetailSync>({ generation: null, sequence: 0 });
  const activeRowRef = useRef<Map<string, { source: ConnectionEntry; row: ConnRow }>>(new Map());
  const closedIndexRef = useRef<Map<string, ClosedConnectionEntry>>(new Map());
  const closedStaticRef = useRef<Map<string, ConnStaticCacheEntry>>(new Map());
  const closedRowRef = useRef<
    Map<string, { source: ClosedConnectionEntry; row: ConnRow }>
  >(new Map());
  /**
   * 已乐观关闭、等后端增量确认消失的连接 id。
   *
   * 光 `setRows(filter)` 挡不住回填：detail 流按1s合并，关闭请求发出时可能已有一帧在途，
   * 那帧仍可能更新这条连接 → 行「关掉又冒回来」再等一秒才真消失。故记一个抑制集，索引里还带着它就滤掉，
   * 直到某一帧里它真没了才把 id 从集里摘掉（自清理，不会无界增长）。
   */
  const closingRef = useRef<Set<string>>(new Set());

  /* ── 行右键菜单（G4，原型 :5051-5055 `contextmenu` on #conn-tbody tr）──
   * 四个动作的后端**全部现成**：复制走 clipboard、加规则走 `rules.add` 的完整弹窗、关闭走既有 onClose。
   * 定位用 `position:fixed` + 视口 clamp 而不是 `.ctx-menu` 自带的 absolute：菜单的宿主
   * `.conn-scroll` 是 `overflow:auto`，absolute 以内容原点为基准、`getBoundingClientRect` 给可视框，
   * 表一滚两者就错位。fixed 没有这个歧义，代价是滚动时要主动关（下方 effect 一并处理）。
   */
  const menuRef = useRef<HTMLDivElement>(null);
  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    row: ConnRow;
    subjects: RuleSubject[];
    subject: RuleSubject | null;
    closable: boolean;
  } | null>(null);
  const [menuSize, setMenuSize] = useState({ w: 0, h: 0 });

  /** 应用 generation/sequence 守卫后的活动连接增量，只重建真正变化的行对象。 */
  const applyDetailUpdate = useCallback((update: ConnectionsDetailUpdate) => {
    const applied = applyActiveDetailUpdate(
      activeIndexRef.current,
      activeSyncRef.current,
      update,
    );
    if (!applied.accepted) return;

    const prev = prevRef.current;
    const staticCache = activeStaticRef.current;
    const rowCache = activeRowRef.current;
    const now = update.at || Date.now();
    if (applied.reset) {
      prev.clear();
      staticCache.clear();
      rowCache.clear();
    }
    for (const id of applied.removedIds) {
      prev.delete(id);
      staticCache.delete(id);
      rowCache.delete(id);
    }

    const closing = closingRef.current;
    for (const id of closing) {
      if (!activeIndexRef.current.has(id)) closing.delete(id);
    }

    const nextRows: ConnRow[] = [];
    for (const entry of activeIndexRef.current.values()) {
      if (closing.has(entry.id)) continue;
      const cached = rowCache.get(entry.id);
      if (cached?.source === entry) {
        nextRows.push(cached.row);
        continue;
      }
      const up = entry.upload ?? 0;
      const dn = entry.download ?? 0;
      const p = prev.get(entry.id);
      let upRate = 0;
      let dnRate = 0;
      if (p && now > p.at) {
        const dt = (now - p.at) / 1000;
        upRate = Math.max(0, (up - p.up) / dt);
        dnRate = Math.max(0, (dn - p.dn) / dt);
      }
      prev.set(entry.id, { up, dn, at: now });
      const row = projectConnection(
        entry,
        null,
        { up: upRate, down: dnRate },
        update.sequence,
        staticCache,
      );
      rowCache.set(entry.id, { source: entry, row });
      nextRows.push(row);
    }
    setRows(nextRows);
    setActiveClock({ at: now, sequence: update.sequence });
    setActiveLoaded(true);
  }, []);

  useEffect(() => {
    if (paused || view !== 'active') return;
    prevRef.current.clear();
    const sub = createTopicSubscription<ConnectionsDetailUpdate>(
      {
        onFrame: (cb) => api.stats.onConnectionsDetail(cb),
        subscribe: () => api.stats.subscribe('detail'),
        unsubscribe: () => api.stats.unsubscribe('detail'),
      },
      applyDetailUpdate
    );
    sub.setWanted(true);
    return () => sub.dispose();
  }, [paused, view, applyDetailUpdate]);

  useEffect(() => {
    if (view === 'active') return;
    closingRef.current.clear();
    prevRef.current.clear();
    activeStaticRef.current.clear();
    activeRowRef.current.clear();
    clearActiveDetailState(activeIndexRef.current, activeSyncRef.current);
    setRows([]);
    setActiveClock({ at: 0, sequence: 0 });
    setActiveLoaded(false);
  }, [view]);

  useEffect(() => {
    if (view !== 'closed') return;
    setClosedLoaded(false);
    const sub = createTopicSubscription<ConnectionsClosedUpdate>(
      {
        onFrame: (cb) => api.stats.onConnectionsClosed(cb),
        subscribe: () => api.stats.subscribe('closed'),
        unsubscribe: () => api.stats.unsubscribe('closed'),
      },
      (update) => {
        setClosedEntries(applyClosedHistoryUpdate(closedIndexRef.current, update));
        setClosedLoaded(true);
      },
    );
    sub.setWanted(true);
    return () => {
      sub.dispose();
      closedIndexRef.current.clear();
      closedStaticRef.current.clear();
      closedRowRef.current.clear();
      setClosedEntries([]);
      setClosedLoaded(false);
    };
  }, [view]);

  /**
   * CLOSED 增量合并后，未变条目保持 `source` 引用，因此整个 ConnRow 也可以复用。
   * 常态新增一条只创建一个行对象，不再每秒重建其余 999 条。
   */
  const closedRows = useMemo(() => {
    const rowCache = closedRowRef.current;
    const staticCache = closedStaticRef.current;
    const liveIds = new Set<string>();
    const next = closedEntries.map((source) => {
      const id = source.entry.id;
      liveIds.add(id);
      const cached = rowCache.get(id);
      if (cached?.source === source) return cached.row;
      const endedAt = Math.max(0, source.closedAt / 1_000_000);
      const row = projectConnection(
        source.entry,
        endedAt,
        { up: 0, down: 0 },
        0,
        staticCache,
      );
      rowCache.set(id, { source, row });
      return row;
    });
    for (const id of rowCache.keys()) {
      if (!liveIds.has(id)) {
        rowCache.delete(id);
        staticCache.delete(id);
      }
    }
    return next;
  }, [closedEntries]);

  /* ── TOP 聚合订阅（切到 top 视图才订，table 视图退订省流）──
   * 同 detail 腿走状态机。这条腿原先连 detail 腿那个 `cancelled` 守卫都没有：tab 连点时 cleanup 跑在
   * `.then()` 之前 → `off` 还是空壳、真监听在 cleanup 之后才注册且**再没人摘**，每点一次漏一个
   * onConnectionsAggregate 监听（漏的监听活到进程结束，此后每帧聚合都白跑一遍全部死回调）。 */
  useEffect(() => {
    if (view !== 'top') return;
    const sub = createTopicSubscription<ConnectionsAggregate>(
      {
        onFrame: (cb) => api.stats.onConnectionsAggregate(cb),
        subscribe: () => api.stats.subscribe('aggregate'),
        unsubscribe: () => api.stats.unsubscribe('aggregate'),
      },
      setAggregate
    );
    sub.setWanted(true);
    return () => sub.dispose();
  }, [view]);

  useEffect(() => {
    setSort(null);
    setMenu(null);
  }, [view]);

  /* ── 暂停切换 ──
   * 只翻标志位；速率记账的清空归订阅腿（恢复订阅时清，见该 effect 内注释）——切回明细视图也是一次
   * 重订阅，两条路径共用同一处清空，不必各写一份。 */
  const togglePause = useCallback(() => setPaused((p) => !p), []);

  /* ── 搜索过滤 + 排序（本地）── */
  const q = search.trim().toLowerCase();
  const matchRow = useCallback(
    (r: ConnRow) =>
      !q ||
      // 搜索纳入 process / network(L4)（对齐 上游）：按进程名 / 传输类型识别连接来源。
      (r.host + r.dest + r.rule + r.chain + r.procName + r.procFull + r.l4Title)
        .toLowerCase()
        .includes(q),
    [q]
  );

  const listRows = view === 'closed' ? closedRows : rows;
  const sortAt = sort?.key === 'time' ? activeClock.at : 0;
  const sortSequence = sort?.key === 'rate' ? activeClock.sequence : 0;

  /** 搜索命中 + 排序后的完整列表；分页只限制 DOM，不截断数据与检索范围。 */
  const filteredRows = useMemo(() => {
    let list = listRows.filter(matchRow);
    if (sort) {
      const { key, dir } = sort;
      const cmp = (a: ConnRow, b: ConnRow): number => {
        switch (key) {
          case 'type':
            return a.l4.localeCompare(b.l4);
          case 'host':
            return a.host.localeCompare(b.host);
          case 'dest':
            return a.dest.localeCompare(b.dest);
          case 'rule':
            return a.rule.localeCompare(b.rule);
          case 'chain':
            return a.chain.localeCompare(b.chain);
          case 'rate': {
            const aRate = connectionRates(a, sortSequence);
            const bRate = connectionRates(b, sortSequence);
            return aRate.down + aRate.up - (bRate.down + bRate.up);
          }
          case 'total':
            return a.total - b.total;
          case 'time':
            return connectionAge(a, sortAt) - connectionAge(b, sortAt);
          case 'ended':
            return (a.endedAt ?? 0) - (b.endedAt ?? 0);
          case 'proc':
            return a.procName.localeCompare(b.procName);
        }
      };
      list = [...list].sort((a, b) => dir * cmp(a, b));
    }
    return list;
  }, [listRows, matchRow, sort, sortAt, sortSequence]);
  const pageCount = Math.max(1, Math.ceil(filteredRows.length / CONNECTION_PAGE_SIZE));
  const visiblePage = Math.min(page, pageCount - 1);
  const pageStart = visiblePage * CONNECTION_PAGE_SIZE;
  const pageEnd = Math.min(filteredRows.length, pageStart + CONNECTION_PAGE_SIZE);
  const visibleRows = filteredRows.slice(pageStart, pageEnd);

  useEffect(() => {
    setPage(0);
  }, [view, q, sort]);

  useEffect(() => {
    if (page !== visiblePage) setPage(visiblePage);
  }, [page, visiblePage]);

  useEffect(() => {
    if (tableScrollRef.current) tableScrollRef.current.scrollTop = 0;
  }, [visiblePage, view, q, sort]);

  /* ── 关单条 / 关全部 ──
   * 后端 connections_close / connections_close_all 已真接管理 API gRPC（commands/proxy.rs:151-188），
   * 失败腿返 err（核未运行 / gRPC 连不上 / 内核拒绝）→ invoke 抛。
   * 不检查结果会让确认弹层关掉、实际啥也没发生，用户被误导。故验 ok + 失败时 toast 报出真实原因
   * （对齐已接的 81 处 toast 套路，原型 :4113/:4114 notify('已关闭全部连接'/'已关闭 N 条连接','ok')；
   * 单条关闭原型 :4115 无 notify——静默移除即反馈，成功不额外 toast，仅失败上报）。
   */
  const closeFailedText = t('connections.closeFailed');

  /* 乐观移除：点了叉立刻走人，别让用户对着一行「关不掉的连接」等下一帧（≤1s）。
   * 入 closingRef 是为了扛住在途增量回填（详见该 ref 的注释）；失败则回滚 —— 暂停态没有后续帧，
   * 不显式放回去的话那条连接就凭空消失了，用户以为关成功了。 */
  const onClose = useCallback(
    async (row: ConnRow) => {
      const id = row.entry.id;
      closingRef.current.add(id);
      // 回滚要插回**原来的位置**，不是追加到表尾：未排序视图下追加会让那一行跳到最底，
      // 用户看到的是「关不掉」+「还换了地方」两件事叠一起。原 index 在乐观移除前先记下。
      let at = -1;
      setRows((prev) => {
        at = prev.findIndex((r) => r.entry.id === id);
        return prev.filter((r) => r.entry.id !== id);
      });
      const rollback = (detail?: string) => {
        closingRef.current.delete(id);
        setRows((prev) => {
          if (prev.some((r) => r.entry.id === id)) return prev;
          const next = [...prev];
          // 期间可能已有增量改变了列表长度 ⇒ 夹取到合法区间；取不到原位就落表尾。
          next.splice(at < 0 ? next.length : Math.min(at, next.length), 0, row);
          return next;
        });
        toast.error(closeFailedText, detail);
      };
      try {
        const res = await api.connections.close(id);
        if (!res?.ok) rollback();
      } catch (err) {
        console.error('[connections] close failed:', err);
        rollback(err instanceof Error ? err.message : undefined);
      }
    },
    [closeFailedText],
  );

  /* ── 行右键菜单：测量 / 关闭 / 定位 / 四个动作 ── */

  /** 浮层尺寸随内容变（域名长短）⇒ 每次换目标后重测，再由 `menuPos` 修正位置。 */
  useLayoutEffect(() => {
    if (!menu || !menuRef.current) return;
    const r = menuRef.current.getBoundingClientRect();
    setMenuSize((p) => (p.w === r.width && p.h === r.height ? p : { w: r.width, h: r.height }));
  }, [menu]);

  /* 点空白 / ESC / 表格滚动 → 关。滚动那条是 fixed 定位的代价：不关的话菜单会浮在原处，
     而它指向的那一行已经滚走了 —— 那比没有菜单更糟（用户会对着错的行下手）。 */
  useEffect(() => {
    if (!menu) return;
    const onDown = (e: MouseEvent) => {
      if (!menuRef.current?.contains(e.target as Node)) setMenu(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenu(null);
    };
    const onScroll = () => setMenu(null);
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    // capture：滚动事件不冒泡，`.conn-scroll` 的滚动只有在捕获阶段才收得到。
    document.addEventListener('scroll', onScroll, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
      document.removeEventListener('scroll', onScroll, true);
    };
  }, [menu]);

  const menuPos = useMemo(() => {
    if (!menu) return null;
    const viewport = { left: 0, top: 0, width: window.innerWidth, height: window.innerHeight };
    return clampToWrap(viewport, menu.x, menu.y, menuSize, 0);
  }, [menu, menuSize]);

  /** 复制到剪贴板（失败给 toast —— 静默失败会让用户以为复制成功，去粘贴才发现是空的）。 */
  const copyText = useCallback(
    async (text: string) => {
      setMenu(null);
      try {
        await navigator.clipboard.writeText(text);
        toast.success(t('connections.copied'));
      } catch {
        toast.error(t('connections.copyFailed'));
      }
    },
    [t],
  );

  /**
   * 批量关闭的抑制集出入口（与单条 `onClose` 同一道防线）。
   *
   * 少了它，批量关闭后**在途的那一帧增量仍可能更新这些行** ⇒ 整批「消失 → 闪回 → 再消失」：
   * 后端已经关掉了、渲染端也已按新帧清空，然后 ≤1s 前就发出的那帧把它们全部画回来，
   * 再等一帧才真消失。单条关闭早就入集了，两条批量腿是漏网的（2026-07-28 复审 LOW）。
   *
   * 成功路径**不需要**显式清：抑制集在增量索引确认 id 已移除时自清理。
   * 失败路径**必须**显式清 —— 否则那条还活着的连接会被永久滤掉（它每帧都在，自清理永远等不到），
   * 用户侧表现为「关失败了，但这条连接再也看不见」。这与单条 `onClose` 的 `rollback` 同款语义。
   */
  const suppressClosing = useCallback((ids: readonly string[]) => {
    for (const id of ids) closingRef.current.add(id);
  }, []);
  const unsuppressClosing = useCallback((ids: readonly string[]) => {
    for (const id of ids) closingRef.current.delete(id);
  }, []);

  /**
   * 两颗批量关闭按钮的原地二次确认 —— 走全仓唯一实现（`lib/confirm-twice.ts`）。
   *
   * 此前这里是**第三套**写法：只有 `onBlur` 复位、**没有** 2.6s 超时。原型 confirmTwice（L3211-3218）
   * 只有超时这一条复位腿，`onBlur` 是本仓自己加的 —— 一并去掉，两颗按钮与日志屏、节点屏同款。
   */
  const { armed, confirmTwice } = useConfirmTwice();
  const confirmingAll = armed === CLOSE_ALL_KEY;
  const confirmingFiltered = armed === CLOSE_FILTERED_KEY;
  const confirmingClear = armed === CLEAR_CLOSED_KEY;

  const onCloseAll = useCallback(async () => {
    // 「全部关闭」的射程是当前索引里的全部连接（`rows` 而非 `filteredRows`：搜索框有内容时
    // 这个按钮关的仍是全部）。发请求**之前**入集：请求在飞期间就可能有增量帧回来。
    const ids = rows.map((r) => r.entry.id);
    suppressClosing(ids);
    try {
      const res = await api.connections.closeAll();
      if (res?.ok) {
        toast.success(t('connections.closeAllDone'));
      } else {
        unsuppressClosing(ids);
        toast.error(closeFailedText);
      }
    } catch (err) {
      console.error('[connections] closeAll failed:', err);
      unsuppressClosing(ids);
      toast.error(closeFailedText, err instanceof Error ? err.message : undefined);
    }
  }, [rows, suppressClosing, unsuppressClosing, closeFailedText, t]);

  /** 关闭当前筛选命中的全部连接（原型 #conn-close-filtered :2012；仅搜索命中时可用，非「全部关闭」）。 */
  const onCloseFiltered = useCallback(async () => {
    // 用 filteredRows 而非 visibleRows：这个按钮的语义是「关闭筛选命中的**全部**连接」，
    // 分页只改变 DOM 行数，不得把动作偷偷降级成「只关当前页的几十条」。
    const n = filteredRows.length;
    // fan-out **之前**批量入抑制集（同 onCloseAll，理由见 suppressClosing）。
    const ids = filteredRows.map((r) => r.entry.id);
    suppressClosing(ids);
    try {
      // 保留 per-id fan-out；只要有一条报 ok:false 就据实提示，不假装全成。
      const results = await Promise.all(
        filteredRows.map((r) => api.connections.close(r.entry.id)),
      );
      if (results.every((r) => r?.ok)) {
        toast.success(t('connections.closeFilteredDone', { n }));
      } else {
        // **只放回失败的那几条**：成功的那些继续被抑制到删除增量追上为止，否则整批一起闪回。
        unsuppressClosing(ids.filter((_, i) => !results[i]?.ok));
        toast.error(closeFailedText);
      }
    } catch (err) {
      console.error('[connections] closeFiltered failed:', err);
      // Promise.all 整体 reject ⇒ 分不出哪几条成了，全部放回（宁可闪一下，也不能让活连接消失）。
      unsuppressClosing(ids);
      toast.error(closeFailedText, err instanceof Error ? err.message : undefined);
    }
  }, [filteredRows, suppressClosing, unsuppressClosing, closeFailedText, t]);

  const onClearClosed = useCallback(async () => {
    try {
      const snapshot = await api.stats.clearClosed();
      closedIndexRef.current.clear();
      closedStaticRef.current.clear();
      closedRowRef.current.clear();
      setClosedEntries([]);
      setClosedLoaded(true);
      if (snapshot.connections.length === 0) {
        toast.success(t('connections.closedCleared'));
      }
    } catch (error) {
      toast.error(
        t('connections.clearClosedFailed'),
        error instanceof Error ? error.message : undefined,
      );
    }
  }, [t]);

  const onSort = useCallback((key: SortKey) => {
    setSort((s) => {
      if (!s || s.key !== key) return { key, dir: 1 };
      return { key, dir: (s.dir === 1 ? -1 : 1) as 1 | -1 };
    });
  }, []);

  const thSortable = (key: SortKey, label: string, extraClass?: string) => (
    <th
      className={`${extraClass ? extraClass + ' ' : ''}sortable${sort?.key === key ? ' sorted' : ''}`}
      onClick={() => onSort(key)}
      data-tip={label}
    >
      <span>{label}</span>
      <span className="sort-ar">{sort?.key === key ? (sort.dir > 0 ? '▲' : '▼') : '▲'}</span>
    </th>
  );

  // TOP 视图数据：按 count 降序截断前 topN（应用 seg2 选择的展示条数）。
  // 先剔除后端的「其它」合并桶（TOPOLOGY_OTHERS_KEY）再排序/截断 —— 否则小 N 下该合成桶会挤掉真实
  // host，用户看不全真实域名（后端 Top-15 之外的连接全被并进这一桶，它的 count 常居高）。
  const hosts = useMemo(
    () =>
      [...(aggregate?.hosts ?? [])]
        .filter((h) => h.name !== TOPOLOGY_OTHERS_KEY)
        .sort((a, b) => b.count - a.count)
        .slice(0, topN),
    [aggregate, topN],
  );
  const outbounds = useMemo(
    () => [...(aggregate?.outbounds ?? [])].sort((a, b) => b.count - a.count).slice(0, topN),
    [aggregate, topN],
  );
  const maxHost = hosts.reduce((m, h) => Math.max(m, h.count), 0) || 1;
  const maxOut = outbounds.reduce((m, o) => Math.max(m, o.count), 0) || 1;

  return (
    <section className="screen" id="s-connections" hidden={false}>
      <div className="phead">
        <div>
          <h1>{t('connections.pageTitle')}</h1>
        </div>
      </div>

      <div className="conn-toolbar">
        {/* 三个视图对应三种真实生命周期；列表页切走即卸载 DOM 与退订。 */}
        <div className="sub-tabs" role="tablist" aria-label={t('connections.active')} style={{ marginBottom: 0 }}>
          <button
            className={view === 'top' ? 'on' : ''}
            role="tab"
            aria-selected={view === 'top'}
            onClick={() => setView('top')}
          >
            <span>{t('home.connectionTopology')}</span>
          </button>
          <button
            className={view === 'active' ? 'on' : ''}
            role="tab"
            aria-selected={view === 'active'}
            onClick={() => setView('active')}
          >
            <span>{t('connections.activeTab')}</span>
          </button>
          <button
            className={view === 'closed' ? 'on' : ''}
            role="tab"
            aria-selected={view === 'closed'}
            onClick={() => setView('closed')}
          >
            <span>{t('connections.closedTab')}</span>
          </button>
        </div>

        {/*
          工具栏后半段：搜索 / 暂停 / 关闭筛选命中 / 全部关闭 —— **四个都只作用于明细表**，
          故整体 gate 在列表视图，拓扑视图下不渲染。
          （默认视图改成拓扑之后它们成了进页第一眼，而在那个视图里全是空按钮：搜索过滤的是表的行、
          暂停控制的是表的订阅腿、两颗关闭按钮的射程都来自表的 `rows`/`filteredRows`。）

          为什么是条件渲染而不是 `hidden`：搜索框是 `<label>` 且带内联 `display:flex`，
          内联样式压过 UA 表的 `[hidden]{display:none}` —— 挂 `hidden` 它照样显示。
          `#conn-close-filtered` 的显隐由活动视图统一控制，
          留着是同一条件的两处副本）。

          搜索词与暂停态**不随隐藏清空**，判据见 `search`/`paused` 声明处。
        */}
        {(view === 'active' || view === 'closed') && (
          <>
            {/* 搜索 */}
            <label className="input" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 11px', cursor: 'text' }}>
              <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8" style={{ color: 'hsl(var(--fg-faint))', flex: 'none' }}>
                <circle cx="11" cy="11" r="7" />
                <path d="M20 20l-3-3" />
              </svg>
              <input
                id="conn-search"
                type="search"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder={t('connections.search')}
                style={{ border: 0, background: 'none', outline: 'none', flex: 1, padding: '8px 0', font: 'inherit', color: 'inherit' }}
              />
            </label>

            {view === 'active' && <>
            <button className="btn ghost" id="conn-pause-btn" onClick={togglePause}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect x="6" y="5" width="4" height="14" rx="1" />
                <rect x="14" y="5" width="4" height="14" rx="1" />
              </svg>
              <span id="conn-pause-lbl">{paused ? t('connections.resume') : t('connections.pause')}</span>
            </button>

            {/* 关闭筛选命中的全部连接：搜索命中非空时才出现（原型 #conn-close-filtered :2012——靠 hidden 切显，两段确认） */}
            <button
              id="conn-close-filtered"
              className={`btn ghost${confirmingFiltered ? ' confirming' : ''}`}
              hidden={!(q && filteredRows.length > 0)}
              style={{ color: 'hsl(var(--err))' }}
              onClick={() => confirmTwice(CLOSE_FILTERED_KEY, () => void onCloseFiltered())}
              data-tip={
                confirmingFiltered
                  ? t('connections.confirm')
                  : t('connections.closeFilteredTitle')
              }
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M4 6h16M7 12h10M10 18h4" />
                <path d="M18 15l4 4M22 15l-4 4" />
              </svg>
              <span id="conn-filtered-lbl">
                {confirmingFiltered
                  ? t('connections.confirm')
                  : t('connections.closeFiltered', {
                      n: filteredRows.length,
                    })}
              </span>
            </button>

            {/* 全部关闭（两段确认：再点一次执行；原型恒红字 ghost，确认态靠 .confirming 类实心翻红，非文字变色） */}
            <button
              className={`btn ghost${confirmingAll ? ' confirming' : ''}`}
              style={{ color: 'hsl(var(--err))' }}
              onClick={() => confirmTwice(CLOSE_ALL_KEY, () => void onCloseAll())}
              data-tip={confirmingAll ? t('connections.confirm') : t('connections.closeAllTitle')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <circle cx="12" cy="12" r="9" />
                <path d="M5 5l14 14" />
              </svg>
              <span>
                {confirmingAll ? t('connections.confirm') : t('connections.closeAll')}
              </span>
            </button>
            </>}
            {view === 'closed' && closedRows.length > 0 && (
              <button
                className={`btn ghost${confirmingClear ? ' confirming' : ''}`}
                style={{ color: 'hsl(var(--err))' }}
                onClick={() => confirmTwice(CLEAR_CLOSED_KEY, () => void onClearClosed())}
                data-tip={
                  confirmingClear
                    ? t('connections.confirmClearClosed')
                    : t('connections.clearClosedTitle')
                }
              >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                  <path d="M4 7h16M9 7V4h6v3M7 7l1 13h8l1-13M10 11v5M14 11v5" />
                </svg>
                <span>
                  {confirmingClear
                    ? t('connections.confirmClearClosed')
                    : t('connections.clearClosed')}
                </span>
              </button>
            )}
          </>
        )}
      </div>

      {/* 活动 / 已结束列表只在当前视图挂载；分页避免超长滚动面留住 graphics surface。 */}
      {(view === 'active' || view === 'closed') && (
      <div id="conn-table-view">
        <div className="conn-scroll" ref={tableScrollRef}>
          <div className="conn-list-wrap">
            <table className={`conn-table conn-table-${view}`}>
              <colgroup>
                {view === 'active' && <col className="c-close" />}
                <col className="c-type" />
                <col className="c-host" />
                <col className="c-dest" />
                <col className="c-rule" />
                <col className="c-chain" />
                {view === 'active' && <col className="c-rate" />}
                <col className="c-total" />
                <col className="c-time" />
                {view === 'closed' && <col className="c-ended" />}
                <col className="c-proc" />
              </colgroup>
              <thead>
                <tr>
                  {view === 'active' && (
                    <th className="c-close" aria-label={t('connections.close')} />
                  )}
                  {thSortable('type', t('connections.colType'), 'c-type')}
                  {thSortable('host', t('connections.colHost'), 'c-host')}
                  {thSortable('dest', t('connections.colDest'), 'c-dest')}
                  {thSortable('rule', t('connections.colRule'), 'c-rule')}
                  {thSortable('chain', t('connections.colChain'), 'c-chain')}
                  {view === 'active' && thSortable('rate', t('connections.colSpeed'), 'c-rate')}
                  {thSortable('total', t('connections.colTraffic'), 'c-total')}
                  {thSortable('time', t('connections.colTime'), 'c-time')}
                  {view === 'closed' && thSortable('ended', t('connections.colEnded'), 'c-ended')}
                  {thSortable('proc', t('connections.colProcess'), 'c-proc')}
                </tr>
              </thead>
              <tbody id="conn-tbody">
                {privacyMode || !(view === 'active' ? activeLoaded : closedLoaded) || visibleRows.length === 0 ? (
                  <tr className="conn-empty-row">
                    <td colSpan={view === 'active' ? 10 : 9}>
                      <div className="stub" style={{ border: 0, padding: 30 }}>
                        <h4>
                          {privacyMode
                            ? t('connections.privacyHidden')
                            : !(view === 'active' ? activeLoaded : closedLoaded)
                              ? t('connections.loading')
                              : listRows.length === 0
                                ? view === 'active'
                                  ? t('connections.noActive')
                                  : t('connections.noClosed')
                              : t('connections.noMatch')}
                        </h4>
                      </div>
                    </td>
                  </tr>
                ) : (
                  <>
                  {visibleRows.map((r) => {
                    const blocked = r.chain === 'block';
                    const direct = r.chain === 'direct';
                    const rates = connectionRates(r, activeClock.sequence);
                    const age = connectionAge(r, activeClock.at);
                    const cx = blocked
                      ? t('home.routingBlock')
                      : direct
                        ? t('home.routingDirect')
                        : r.chain;
                    return (
                      <tr
                        className="conn-data-row"
                        key={r.entry.id}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          const subjects = connectionRuleSubjects(r.entry);
                          setMenuSize({ w: 0, h: 0 }); // 换行即重测，别拿上一行的尺寸定位
                          setMenu({
                            x: e.clientX,
                            y: e.clientY,
                            row: r,
                            subjects,
                            subject: subjects[0] ?? null,
                            closable: view === 'active',
                          });
                        }}
                      >
                        {view === 'active' && (
                        <td className="c-close">
                          <button
                            className="conn-x"
                            onClick={() => void onClose(r)}
                            data-tip={t('connections.close')}
                            aria-label={t('connections.close')}
                          >
                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                              <path d="M5 5l14 14M19 5L5 19" />
                            </svg>
                          </button>
                        </td>
                        )}
                        <td className="c-type">
                          <span className={`pill ${r.udp ? 'udp' : 'tcp'}`} data-tip={r.l4Title}>
                            {r.l4}
                          </span>
                        </td>
                        {/* 域名/目标/规则/节点链四列都可能很长（规则条件、机场节点名、长域名），
                            此前只有域名与进程截断，规则与节点链**没有任何宽度上限** ⇒ 一条长规则
                            把整表横向撑开、其余列被挤到视口外（陈先生 2026-07-29 真机报）。
                            统一形态：列定宽 + `.conn-clip` 截断 + `data-tip` 悬停浮窗给全文
                            （tooltip 引擎自带停留延迟，与 `.conn-proc` 既有做法同款）。 */}
                        <td className="c-host">
                          <div className="conn-host" data-tip={r.host !== '—' ? r.host : undefined}>
                            {r.host}
                          </div>
                          {r.entry.metadata?.sourceIP && !privacyMode && (
                            <div className="conn-sub">{r.entry.metadata.sourceIP}</div>
                          )}
                        </td>
                        <td className="mono conn-sub c-dest">
                          <span className="conn-clip" data-tip={r.dest !== '—' ? r.dest : undefined}>
                            {r.dest}
                          </span>
                        </td>
                        <td className="conn-chain c-rule">
                          <span className="conn-clip" data-tip={r.rule !== '—' ? r.rule : undefined}>
                            {r.rule}
                          </span>
                        </td>
                        <td className="conn-chain c-chain">
                          <span className="conn-clip" data-tip={r.chain !== '—' ? r.chain : undefined}>
                            {blocked ? (
                              <span style={{ color: 'hsl(var(--err))' }}>{cx}</span>
                            ) : direct ? (
                              <span style={{ color: 'hsl(var(--fg-dim))' }}>{cx}</span>
                            ) : (
                              <b>{r.chain}</b>
                            )}
                          </span>
                        </td>
                        {view === 'active' && <td className="conn-rate mono c-rate">
                          <span className="d">{fmtRate(rates.down)}</span>{' '}
                          <span className="u">{fmtRate(rates.up)}</span>
                        </td>}
                        <td className="mono conn-sub c-total">{fmtBytes(r.total)}</td>
                        <td className="mono conn-sub c-time">{fmtDuration(age)}</td>
                        {view === 'closed' && (
                          <td className="mono conn-sub c-ended">
                            <span data-tip={r.endedAt ? new Date(r.endedAt).toLocaleString() : undefined}>
                              {r.endedAt
                                ? new Date(r.endedAt).toLocaleString(undefined, {
                                    month: '2-digit',
                                    day: '2-digit',
                                    hour: '2-digit',
                                    minute: '2-digit',
                                    second: '2-digit',
                                  })
                                : '—'}
                            </span>
                          </td>
                        )}
                        <td className="c-proc">
                          <span className="conn-proc" data-tip={r.procFull || undefined}>{r.procName}</span>
                        </td>
                      </tr>
                    );
                  })
                  }
                  </>
                )}
              </tbody>
            </table>
          </div>
        </div>
        {filteredRows.length > CONNECTION_PAGE_SIZE && (
          <div className="conn-pager" aria-live="polite">
            <span>
              {t('connections.pageStatus', {
                start: pageStart + 1,
                end: pageEnd,
                total: filteredRows.length,
              })}
            </span>
            <div className="conn-pager-actions">
              <button
                type="button"
                className="btn ghost conn-page-btn"
                disabled={visiblePage === 0}
                onClick={() => setPage((current) => Math.max(0, current - 1))}
              >
                {t('connections.previousPage')}
              </button>
              <button
                type="button"
                className="btn ghost conn-page-btn"
                disabled={visiblePage >= pageCount - 1}
                onClick={() => setPage((current) => Math.min(pageCount - 1, current + 1))}
              >
                {t('connections.nextPage')}
              </button>
            </div>
          </div>
        )}
        {/* 行右键菜单：域名/IP/进程先选一个规则对象，复制、新建、追加三条动作共用该对象。 */}
        {menu && (
          <div
            ref={menuRef}
            className="ctx-menu"
            style={{ position: 'fixed', ...(menuPos ?? { left: 0, top: 0, opacity: 0 }) }}
          >
            {menu.subject && (
              <>
                <div className="ctx-subject" data-tip={menu.subject.detail || menu.subject.value}>
                  <div
                    className="ctx-subject-tabs"
                    role="group"
                    aria-label={t('connections.ruleSubject')}
                  >
                    {menu.subjects.map((subject) => (
                      <button
                        key={subject.kind}
                        type="button"
                        className={subject.kind === menu.subject?.kind ? 'on' : undefined}
                        aria-pressed={subject.kind === menu.subject?.kind}
                        onClick={() => setMenu((current) => (current ? { ...current, subject } : null))}
                      >
                        {t(`connections.ruleSubjects.${subject.kind}`)}
                      </button>
                    ))}
                  </div>
                  <span className="ctx-subject-value">{menu.subject.value}</span>
                </div>
                <button
                  type="button"
                  className="ctx-i"
                  onClick={() => void copyText(menu.subject!.value)}
                >
                  <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <path d="M9 9h10v10H9zM5 15V5h10" />
                  </svg>
                  {t('connections.copySubject', {
                    type: t(`connections.ruleSubjects.${menu.subject.kind}`),
                  })}
                </button>
                <RuleSubjectMenuItems subject={menu.subject} onDone={() => setMenu(null)} />
              </>
            )}
            {menu.closable && <button
              type="button"
              className="ctx-i danger"
              onClick={() => {
                const row = menu.row;
                setMenu(null);
                void onClose(row);
              }}
            >
              <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M5 5l14 14M19 5L5 19" />
              </svg>
              {t('connections.close')}
            </button>}
          </div>
        )}
      </div>
      )}

      {/* TOP 拓扑视图 */}
      {view === 'top' && <div id="conn-top-view">
        {/* 条数选择器**收进卡片标题行**（陈先生 2026-07-30：「展示前 5 10 15 域名 / 出站 应该在同一行，
            展示前 / 域名 / 出站 这些可以不用显示，只显示数量」）。
            原先它独占一行 `.conn-toolbar`，带两句纯复述的文字：「展示前」与下方卡片里的「前 N」pill 同义，
            「域名 / 出站」与两张卡片自己的标题同义 —— 删掉不丢任何信息，省掉一整行垂直空间。
            控件落在**域名卡**（TOP 视图里主要看的那张）；出站卡的标题保留只读 pill 显示同一个 N，
            让「它俩受同一个开关控制」这件事在视觉上成立 —— 否则控件只出现在一张卡上，会被读成只管那张。 */}
        <div className="top-grid">
          <div className="card pad">
            <div className="card-h top-card-h" style={{ marginBottom: 12 }}>
              <span>{t('connections.topHostsTitle')}</span>
              {/* 原型 :2030 拆「前」标签 + 纯数字 #top-host-n 两节点；id 保留挂点（现由 seg2 承载取值） */}
              <div className="seg2" role="group" aria-label={t('connections.topCount')} id="top-host-n">
                {TOP_N_OPTIONS.map((n) => (
                  <button
                    key={n}
                    type="button"
                    className={topN === n ? 'on' : ''}
                    onClick={() => setTopN(n)}
                  >
                    {n}
                  </button>
                ))}
              </div>
            </div>
            <div id="top-hosts">
              {/* 隐私态：TOP 域名同属「域名」敏感数据 → 隐藏（对齐 privacyHidden 文案 + 表视图脱敏一致）。 */}
              {privacyMode ? (
                <div className="card-sub">{t('connections.privacyHidden')}</div>
              ) : hosts.length === 0 ? (
                <div className="card-sub">{t('connections.noActive')}</div>
              ) : (
                hosts.map((h) => (
                  <div className="top-bar-row" key={h.name}>
                    <span className="tb-name" data-tip={h.name === TOPOLOGY_OTHERS_KEY ? t('home.others') : h.name}>
                      {h.name === TOPOLOGY_OTHERS_KEY ? t('home.others') : h.name}
                    </span>
                    <span className="bar">
                      {/* 原型 renderTopN :3769 host 条恒 --aurora，非默认 .bar>i 的 --flow */}
                      <i style={{ width: `${(h.count / maxHost) * 100}%`, background: 'hsl(var(--aurora))' }} />
                    </span>
                    <span className="tb-v">{h.count}</span>
                  </div>
                ))
              )}
            </div>
          </div>
          <div className="card pad">
            <div className="card-h top-card-h" style={{ marginBottom: 12 }}>
              <span>{t('connections.topOutboundsTitle')}</span>
              {/* 只读：与域名卡的 seg2 同一个 `topN`。不做成第二个 seg2 —— 两份控件绑同一状态，
                  用户会问「这两个有什么区别」，而答案是「没有」。 */}
              <span className="pill region">
                {t('connections.topBadge', { n: topN })}
              </span>
            </div>
            <div id="top-outbounds">
              {outbounds.length === 0 ? (
                <div className="card-sub">—</div>
              ) : (
                outbounds.map((o) => {
                  const isDirect = o.name === 'Direct';
                  const label = isDirect ? t('home.routingDirect') : o.name;
                  return (
                    <div className="top-bar-row" key={o.name}>
                      <span className="tb-name" data-tip={label}>{label}</span>
                      <span className="bar">
                        {/* 原型 renderTopN :3771 出站条按身份配色：直连 --fg-faint / 具名出站 --flow */}
                        <i
                          style={{
                            width: `${(o.count / maxOut) * 100}%`,
                            background: isDirect ? 'hsl(var(--fg-faint))' : 'hsl(var(--flow))',
                          }}
                        />
                      </span>
                      <span className="tb-v">{o.count}</span>
                    </div>
                  );
                })
              )}
            </div>
          </div>
        </div>
      </div>}
    </section>
  );
}

export default ConnectionsScreen;
