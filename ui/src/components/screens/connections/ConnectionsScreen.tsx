/**
 * Connections 屏（逐元素复现原型 polaris-prototype.html #s-connections L2000-2040 +
 * 动态行模板 renderConn/renderTopN L5037-5047/3765-3771，rebuild-plan B3）。
 *
 * 结构对齐原型：
 *  - .phead（标题）
 *  - .conn-toolbar（明细/TOP tab + 搜索 + 暂停 + 关闭全部）
 *  - #conn-table-view（.conn-scroll > .conn-list-wrap > table.conn-table：域名/目标/规则/出站链/上下行/累计/时长 + 关闭列，横向滚动）
 *  - #conn-top-view（Top-N 域名 + 出站分布，.top-grid）
 *
 * 功能接 api-client：
 *  - 明细：statsApi.subscribe('detail') + onConnectionsDetail（订阅驱动，进页订/离开退；
 *    后端 relay 见 runtime/stats.rs `run_detail_poller`——1s 轮询管理 API 连接快照逐帧下发）
 *  - TOP：statsApi.subscribe('aggregate') + onConnectionsAggregate
 *  - 关单条：connectionsApi.close(id)（真调管理 API gRPC CloseConnection）+ 乐观移除（失败回滚）
 *  - 关全部：connectionsApi.closeAll()（真调 CloseAllConnections）
 *  - 暂停：**退订**冻结（不是只冻渲染）——暂停即 unsubscribe('detail')，后端据订阅集降 worker demand，
 *    整条 1s 轮询 + 逐帧序列化链路停机；恢复即重订，下一帧（≤1s）回填。
 *    **切到拓扑视图是同一次退订**：detail 腿同时 gate 在 `view === 'table'`（它的产物只有表视图消费）。
 *    故工具栏里只作用于明细表的三个控件（搜索 / 暂停 / 关闭全部）在拓扑视图下一并隐掉，
 *    判据见 `.conn-toolbar` 处注释。
 *  - 排序：全部 9 个数据列本地可排序（rate/total 需前帧 diff 算速率）
 *  - 截断：过滤 + 排序**之后**取前 500 行渲染（数千连接时全量 .map 撑爆 DOM）
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
  ConnectionEntry,
  ConnectionsSnapshot,
  ConnectionsAggregate,
} from '@/contracts/types';
import { TOPOLOGY_OTHERS_KEY } from '@/contracts/types';
import { fmtBytes, fmtDuration, fmtRate, ageFromStart } from '../shared/format';
import { connectionRuleSubjects } from './connection-rule-subjects';

type ConnView = 'table' | 'top';

/** 本屏两个原地二次确认项（原型 :4113 `conn-close-all` / :4114 `conn-close-filtered`）。 */
const CLOSE_ALL_KEY = 'conn-close-all';
const CLOSE_FILTERED_KEY = 'conn-close-filtered';
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
  | 'proc';

/**
 * 单帧最多渲染的行数（对齐 上游 `MAX_VISIBLE_ROWS`，connections-table.tsx:317）。
 *
 * TUN / BT 场景下活动连接可达数千，全量 `.map` 出的 DOM 行每秒重渲一次直接拖垮主线程。
 * `<table>` 语义下真虚拟化要破坏表结构、收益有限，故取「截断 + 明示提示 + 引导用搜索缩小」这一务实解。
 * **截断发生在过滤与排序之后**：先截再排会让「按流量排序」只在随机的前 500 条里排，排序结果是错的。
 */
const MAX_VISIBLE_ROWS = 500;
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
  /** 连接时长（秒）。 */
  age: number;
}

export function ConnectionsScreen() {
  const { t } = useTranslation();
  const privacyMode = useAppStore((s) => s.privacyMode);

  /**
   * 进页默认视图 = 拓扑（tab 顺序亦为「拓扑 | 明细」）。
   *
   * 订阅影响仅一侧：aggregate 腿 gate 在 `view === 'top'`，故默认拓扑 ⇒ 进页即订 aggregate。
   * 不空屏的依据在后端：`run_aggregate_poller` 的首拍不 sleep（`PollGate::next_tick` 的 `ticked`
   * 分支），且它的内容签名去重状态 `last_sig` 是 poller 任务的局部量 —— 上一屏（Home 拓扑）离开时
   * 订阅计数归零、poller 停机，本页订阅重新起一个 ⇒ `last_sig` 从 None 开始，首个聚合必推。
   *
   * detail 腿**同样** gate 在 view（`view === 'table'`）：它的产物 `rows` / `filteredRows` / `total`
   * 只有表视图消费，拓扑视图下让那条 1s 全量连接快照继续跑 = 每进一次连接页白付一份后端序列化 +
   * IPC + 前端 diff。两条腿各订各的视图，同一时刻只有一条在跑。
   */
  const [view, setView] = useState<ConnView>('top');
  /**
   * 搜索词 / 暂停态：切到拓扑视图时**保留**（控件隐掉，state 不清）。
   *
   * 判据是「控件与它的效果同进同出」——两者都只作用于明细表，随明细表一起消失、一起回来：
   * 切回明细时搜索框带着原来的词回来、暂停按钮带着「继续」字样回来，**不存在「效果还在但控件不见了」
   * 这种看不见的残留态**。反过来切走即清才是问题：那等于用一次 tab 点击悄悄丢掉用户输入
   * （不可撤销），也让「切走再切回」变成一个隐藏的重置手势。
   *
   * `paused` 在拓扑视图下不再有「无法恢复订阅」的隐患 —— detail 腿本就 gate 在 `view === 'table'`，
   * 拓扑视图下无论暂停与否都是退订态，那里没有可恢复的东西。
   */
  const [search, setSearch] = useState('');
  const [paused, setPaused] = useState(false);
  const [sort, setSort] = useState<SortState | null>(null);

  const [rows, setRows] = useState<ConnRow[]>([]);
  const [total, setTotal] = useState(0);
  const [aggregate, setAggregate] = useState<ConnectionsAggregate | null>(null);
  const [topN, setTopN] = useState<number>(10);

  // 上一帧字节记账（算速率）：id → {up, dn, at(ms)}
  const prevRef = useRef<Map<string, { up: number; dn: number; at: number }>>(
    new Map()
  );
  /**
   * 已乐观关闭、等后端快照确认消失的连接 id。
   *
   * 光 `setRows(filter)` 挡不住回填：detail 轮询 1s 一帧，关闭请求发出时可能已有一帧在途，
   * 那帧仍含这条连接 → 行「关掉又冒回来」再等一秒才真消失。故记一个抑制集，快照里还带着它就滤掉，
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
  } | null>(null);
  const [menuSize, setMenuSize] = useState({ w: 0, h: 0 });

  /** 把 ConnectionsSnapshot 派生为 ConnRow[]（算速率 / 时长），写 rows。 */
  const applySnapshot = useCallback((snap: ConnectionsSnapshot) => {
    const prev = prevRef.current;
    const now = snap.at || Date.now();
    // 同一趟投影顺手建 live id 集；此前又对原快照 `.map` 一遍再交给 Set，每秒额外制造一份 N 长数组。
    const liveIds = new Set<string>();
    const nextRows: ConnRow[] = snap.connections.map((entry) => {
      liveIds.add(entry.id);
      const up = entry.upload ?? 0;
      const dn = entry.download ?? 0;
      const host =
        entry.metadata?.host || entry.metadata?.destinationIP || '—';
      const dest =
        entry.metadata?.destinationIP
          ? `${entry.metadata.destinationIP}${
              entry.metadata.destinationPort
                ? ':' + entry.metadata.destinationPort
                : ''
            }`
          : '—';
      const chain = entry.chains?.[0] ?? '—';
      // L4 类型（对齐 上游 typeOf：network 优先，回落 inbound type）+ 进程名（processPath basename）。
      const network = (entry.metadata?.network || '').toLowerCase();
      const l4Parts = [entry.metadata?.network, entry.metadata?.type].filter(Boolean);
      const l4Title = l4Parts.length ? l4Parts.join('/') : '—';
      const l4 = entry.metadata?.network || entry.metadata?.type || '—';
      const procFull = entry.metadata?.processPath || '';
      const procName = procFull ? procFull.split(/[/\\]/).pop() || procFull : '—';
      const p = prev.get(entry.id);
      let upRate = 0;
      let dnRate = 0;
      if (p && now > p.at) {
        const dt = (now - p.at) / 1000;
        upRate = Math.max(0, (up - p.up) / dt);
        dnRate = Math.max(0, (dn - p.dn) / dt);
      }
      prev.set(entry.id, { up, dn, at: now });
      return {
        entry,
        host,
        dest,
        rule: entry.rule ? `${entry.rule}${entry.rulePayload ? ' ' + entry.rulePayload : ''}` : '—',
        chain,
        l4,
        l4Title,
        udp: network === 'udp',
        procName,
        procFull,
        total: up + dn,
        upRate,
        dnRate,
        age: ageFromStart(entry.start) ?? 0,
      };
    });
    // 清理已断开连接的记账
    for (const id of prev.keys()) {
      if (!liveIds.has(id)) prev.delete(id);
    }
    // 乐观关闭的抑制集：本帧还带着的继续滤掉；本帧已没有的说明后端真关掉了 → 从集里摘除（自清理）。
    const closing = closingRef.current;
    if (closing.size > 0) {
      for (const id of [...closing]) {
        if (!liveIds.has(id)) closing.delete(id);
      }
    }
    const visible =
      closing.size > 0
        ? nextRows.filter((r) => !closing.has(r.entry.id))
        : nextRows;
    setRows(visible);
    // total 取**过滤后**的条数：空表文案据它区分「暂无活动连接」与「没有匹配的连接」，
    // 若用原始快照长度，关掉最后一条连接的那一秒会误显示成「没有匹配的连接」。
    setTotal(visible.length);
  }, []);

  /* ── 明细订阅：**表视图**订，切走 / 暂停即退订（不是只冻渲染）──
   *
   * gate 是两维：`view === 'table'` 且未暂停。加 view 这一维的理由是它的产物只有表视图消费
   * （`rows` / `filteredRows` / `total`），拓扑视图下继续跑就是每进一次连接页白付一份全量快照的
   * 序列化 + IPC + 前端 diff。两维是同一件事的两个开关，走同一条退订腿。
   *
   * **切走时不清 `rows`**（切回来先看到旧表，下一帧覆盖）：
   *  - 一致性：暂停走的就是同一条退订腿，而暂停的语义**恰恰是**「把表冻住给我看」——同一个动作
   *    两种缓存策略会让「暂停」与「切走」变成两套语义。
   *  - 陈旧窗口很短：后端 `run_detail_poller` 首拍不 sleep（`PollGate::next_tick` 的 `ticked` 分支），
   *    重订后首帧一趟 IPC 就回来，不是等满 1s。
   *  - 反面更糟：清空后那一小段里表是空的，而空表文案写着「暂无活动连接」——那是一句**假话**
   *    （连接一直在），比短暂陈旧更误导，还多一次闪动。
   *
   * 但速率记账 `prevRef` **必须清**（见下），否则回来第一帧的速率 = 字节差 / 离开时长。
   *
   * 契约是「退订冻结 / 重订恢复」：后端 stats 订阅集是 worker demand 的源，退订 → 1s 轮询管理 API +
   * 逐帧序列化 relay 整条链路停机。只冻前端渲染的话，数千连接的快照照样每秒序列化 + 过 IPC，
   * 「暂停」省的只有一次 setState —— 用户按暂停正是因为机器被这条链路拖住了。
   *
   * 退订期间没有帧可缓存，故不留「暂停帧」缓冲：恢复即重订，下一帧（≤1s）自然回填。
   *
   * 生命周期交给 `createTopicSubscription`（与首页拓扑腿同一份状态机）。**它守的第一条不变式正是
   * 本页「进页面要等一下才出连接」的成因**：后端 `run_detail_poller` 的首拍不 sleep
   * （`PollGate::next_tick` 的 `ticked` 分支），订阅一落地就发首帧；而监听若挂在 `subscribe()` 的
   * `.then()` 里，要多等「invoke 应答回 JS」+「`plugin:event|listen` 再往返一次」两趟才注册得上，
   * 那一帧必然打在没有监听的窗口上被丢掉 → 白等一整拍（1s）才见第一屏数据。
   * 换成状态机后 `plugin:event|listen` 的 invoke 排在 `stats_subscribe` **之前**投递，
   * 同一条 IPC 通道按序处理 ⇒ 首帧必被收到，这个窗口是被关掉而不是被收窄。
   *
   * 暂停/恢复走「整条 dispose + 重建」而非 `setWanted(false)`：dispose **当场摘监听**，
   * 而 `setWanted(false)` 只发退订、监听照旧挂着 —— 后端要等这趟 IPC 落地才停推，那期间的帧仍会
   * 落到表上，用户按下暂停后还能看见表再跳一两次。 */
  useEffect(() => {
    if (paused || view !== 'table') return;
    /* 清速率记账：退订期没有帧，回来后首帧与退订前那帧的 dt = 整个离开/暂停时长，算出来的是
       「跨越空窗的平均速率」——既不是当前速率也不是历史速率。清掉即让首帧显 0，下一帧（≤1s）起
       恢复真实速率。放在**订阅这一侧**而非按钮回调里：清空的前提是「刚重新订上」，暂停恢复与
       切回明细是同一件事，让唯一的订阅点负责，就不会有第二条腿忘了清。 */
    prevRef.current.clear();
    const sub = createTopicSubscription<ConnectionsSnapshot>(
      {
        onFrame: (cb) => api.stats.onConnectionsDetail(cb),
        subscribe: () => api.stats.subscribe('detail'),
        unsubscribe: () => api.stats.unsubscribe('detail'),
      },
      applySnapshot
    );
    sub.setWanted(true);
    return () => sub.dispose();
  }, [paused, view, applySnapshot]);

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

  /** 搜索命中 + 排序后的**完整**列表（未截断）。截断只作用于渲染，不作用于计数与批量关闭。 */
  const filteredRows = useMemo(() => {
    let list = rows.filter(matchRow);
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
          case 'rate':
            return a.dnRate + a.upRate - (b.dnRate + b.upRate);
          case 'total':
            return a.total - b.total;
          case 'time':
            return a.age - b.age;
          case 'proc':
            return a.procName.localeCompare(b.procName);
        }
      };
      list = [...list].sort((a, b) => dir * cmp(a, b));
    }
    return list;
  }, [rows, matchRow, sort]);

  /** 实际渲染的行：过滤 + 排序**之后**截前 500（顺序颠倒会让排序只在随机子集内成立）。 */
  const visibleRows = useMemo(
    () =>
      filteredRows.length > MAX_VISIBLE_ROWS
        ? filteredRows.slice(0, MAX_VISIBLE_ROWS)
        : filteredRows,
    [filteredRows],
  );

  /* ── 关单条 / 关全部 ──
   * 后端 connections_close / connections_close_all 已真接管理 API gRPC（commands/proxy.rs:151-188），
   * 失败腿返 err（核未运行 / gRPC 连不上 / 内核拒绝）→ invoke 抛。
   * 不检查结果会让确认弹层关掉、实际啥也没发生，用户被误导。故验 ok + 失败时 toast 报出真实原因
   * （对齐已接的 81 处 toast 套路，原型 :4113/:4114 notify('已关闭全部连接'/'已关闭 N 条连接','ok')；
   * 单条关闭原型 :4115 无 notify——静默移除即反馈，成功不额外 toast，仅失败上报）。
   */
  const closeFailedText = t('connections.closeFailed');

  /* 乐观移除：点了叉立刻走人，别让用户对着一行「关不掉的连接」等下一帧（≤1s）。
   * 入 closingRef 是为了扛住在途快照回填（详见该 ref 的注释）；失败则回滚 —— 暂停态没有后续快照，
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
          // 期间可能又有整帧快照回填过（长度变了）⇒ 夹取到合法区间；取不到原位就落表尾。
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
   * 少了它，批量关闭后**在途的那一帧快照仍然含这些行** ⇒ 整批「消失 → 闪回 → 再消失」：
   * 后端已经关掉了、渲染端也已按新帧清空，然后 ≤1s 前就发出的那帧把它们全部画回来，
   * 再等一帧才真消失。单条关闭早就入集了，两条批量腿是漏网的（2026-07-28 复审 LOW）。
   *
   * 成功路径**不需要**显式清：抑制集自清理（`applySnapshot` 里某帧不再含该 id 即摘除）。
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

  const onCloseAll = useCallback(async () => {
    // 「全部关闭」的射程是当前快照里的全部连接（`rows` 而非 `filteredRows`：搜索框有内容时
    // 这个按钮关的仍是全部）。发请求**之前**入集：请求在飞期间就可能有快照帧回来。
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
    // 500 行截断是渲染上限，不该把它偷偷降级成「关掉看得见的那 500 条」。
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
        // **只放回失败的那几条**：成功的那些继续被抑制到快照追上为止，否则整批一起闪回。
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
        {/* TOP 拓扑 / 明细 tab（拓扑在前且为默认视图，见 `view` 初值注释） */}
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
            className={view === 'table' ? 'on' : ''}
            role="tab"
            aria-selected={view === 'table'}
            onClick={() => setView('table')}
          >
            <span>{t('connections.detailTab')}</span>
          </button>
        </div>

        {/*
          工具栏后半段：搜索 / 暂停 / 关闭筛选命中 / 全部关闭 —— **四个都只作用于明细表**，
          故整体 gate 在 `view === 'table'`，拓扑视图下不渲染。
          （默认视图改成拓扑之后它们成了进页第一眼，而在那个视图里全是空按钮：搜索过滤的是表的行、
          暂停控制的是表的订阅腿、两颗关闭按钮的射程都来自表的 `rows`/`filteredRows`。）

          为什么是条件渲染而不是 `hidden`：搜索框是 `<label>` 且带内联 `display:flex`，
          内联样式压过 UA 表的 `[hidden]{display:none}` —— 挂 `hidden` 它照样显示。
          `#conn-close-filtered` 原先自带的 `view === 'table' &&` 一并去掉（被本 gate 覆盖，
          留着是同一条件的两处副本）。

          搜索词与暂停态**不随隐藏清空**，判据见 `search`/`paused` 声明处。
        */}
        {view === 'table' && (
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

            {/* 暂停（原型 #conn-pause-btn :2011——图标恒为暂停条，仅文案 暂停⇄继续 切换，不换成播放三角） */}
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
          </>
        )}
      </div>

      {/* 明细表视图 */}
      <div id="conn-table-view" hidden={view !== 'table'}>
        <div className="conn-scroll">
          <div className="conn-list-wrap">
            <table className="conn-table">
              <thead>
                <tr>
                  <th className="c-close" aria-label={t('connections.close')} />
                  {/* Type(L4) 首数据列 + Process 末列——对齐 上游 连接表（原型缺此二列，按功能 oracle 补）。
                      close 之外每个数据列都可排序（契约的 8 列 + Polaris 多出的 Process）。 */}
                  {thSortable('type', t('connections.colType'), 'c-type')}
                  {thSortable('host', t('connections.colHost'))}
                  {thSortable('dest', t('connections.colDest'), 'c-dest')}
                  {thSortable('rule', t('connections.colRule'), 'c-rule')}
                  {thSortable('chain', t('connections.colChain'), 'c-chain')}
                  {thSortable('rate', t('connections.colSpeed'), 'c-rate')}
                  {thSortable('total', t('connections.colTraffic'), 'c-total')}
                  {thSortable('time', t('connections.colTime'), 'c-time')}
                  {thSortable('proc', t('connections.colProcess'), 'c-proc')}
                </tr>
              </thead>
              <tbody id="conn-tbody">
                {/* 隐私态整表隐藏：只留「隐私模式下不可用」占位——host/dest/sourceIP 全不渲染（完整脱敏，
                    对齐 privacyHidden 文案）。此前仅 hidden sourceIP、host/dest 仍露 = 不完整，已补。 */}
                {privacyMode || visibleRows.length === 0 ? (
                  <tr>
                    <td colSpan={10}>
                      <div className="stub" style={{ border: 0, padding: 30 }}>
                        <h4>
                          {privacyMode
                            ? t('connections.privacyHidden')
                            : total === 0
                              ? t('connections.noActive')
                              : t('connections.noMatch')}
                        </h4>
                      </div>
                    </td>
                  </tr>
                ) : (
                  visibleRows.map((r) => {
                    const blocked = r.chain === 'block';
                    const direct = r.chain === 'direct';
                    const cx = blocked
                      ? t('home.routingBlock')
                      : direct
                        ? t('home.routingDirect')
                        : r.chain;
                    return (
                      <tr
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
                          });
                        }}
                      >
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
                        <td>
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
                        <td className="conn-rate mono c-rate">
                          <span className="d">{fmtRate(r.dnRate)}</span>{' '}
                          <span className="u">{fmtRate(r.upRate)}</span>
                        </td>
                        <td className="mono conn-sub c-total">{fmtBytes(r.total)}</td>
                        <td className="mono conn-sub c-time">{fmtDuration(r.age)}</td>
                        <td className="c-proc">
                          <span className="conn-proc" data-tip={r.procFull || undefined}>{r.procName}</span>
                        </td>
                      </tr>
                    );
                  })
                )}
              </tbody>
            </table>
          </div>
        </div>
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
            <button
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
            </button>
          </div>
        )}
        {/* 500 行截断提示：常驻滚动区**外**（卡底），不然它自己也被滚走 = 用户永远看不到「还有几千条没显示」。
            隐私态整表不渲染 → 不提。 */}
        {!privacyMode && filteredRows.length > MAX_VISIBLE_ROWS && (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 6,
              flex: 'none',
              marginTop: 8,
              fontSize: 11.5,
              color: 'hsl(var(--warn))',
            }}
          >
            <svg
              viewBox="0 0 24 24"
              width="14"
              height="14"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.9"
              style={{ flex: 'none' }}
            >
              <path
                d="M12 9v4M12 17h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
            <span>
              {t('connections.rowsTruncated', {
                shown: MAX_VISIBLE_ROWS,
                total: filteredRows.length,
              })}
            </span>
          </div>
        )}
      </div>

      {/* TOP 拓扑视图 */}
      <div id="conn-top-view" hidden={view !== 'top'}>
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
      </div>
    </section>
  );
}

export default ConnectionsScreen;
