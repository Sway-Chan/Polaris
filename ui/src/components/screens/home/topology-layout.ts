/**
 * 连接拓扑（首页 Sankey）布局计算 —— 纯函数，无 React/Tauri 依赖，可单独 import。
 *
 * 几何与配色以 Polaris 原型为准（polaris-prototype.html:3506-3535 `renderSankey`）：
 * 条宽 8、三列 x = 0.12/0.50/0.88·W、列标签（设备/活跃目标/出站）、左 aurora·中 flow·右 flow（直连 fg-faint）。
 *
 * 缩放算法取上游 issue #303 的定稿（vault 拓扑图那份记录的题 1B），不取原型：
 * 原型「按比例分配 + 最小 16px」只在 3 个目标的 mock 下成立。默认 16 槽满载时，若仍用该算法会达到 408px
 * 会溢出 ~298px 画布；且原型左条满高时 N=1 出现「左 298 vs 中 16」失衡。issue303 经 5 轮出图定稿为
 * 三列各自独立 scale：左条恒 SOURCE_HEIGHT（设备只有一个，不表达流量），中/右列取 min(每条上限, 本列总量上限/总数)。
 *
 * 三列互不守恒是**有意为之**：缎带两端本就按各自列的比例独立算（heightSource/heightTarget），异 scale 不影响缎带正确性。
 */
import { TOPOLOGY_OTHERS_KEY, type ConnectionsAggregate } from '@/contracts/types';

export interface TopoNode {
  id: string;
  name: string;
  type: 'source' | 'host' | 'outbound';
  value: number;
  x: number;
  y: number;
  height: number;
  /** 条填充色（原型用 inline hsl(var(--token))：node.color 是运行时值，Tailwind 静态扫描不到 fill-* 类）。 */
  color: string;
  /**
   * 该 host 是否为「其它」聚合 sentinel（超出当前画布容量后合并组）。
   * 组件据此禁掉右键「加规则」——它是聚合占位、名字是本地化文案而非真实域名，为它加 domainSuffix 规则会写垃圾。
   * 对齐 上游 护栏（issue303:199：仅中列 rule 节点、排除「其它」、名字须含 `.`/`:`）。source/outbound 恒 false。
   */
  isOthers: boolean;
  /** 后端动态投影选出的最近活动目标；只可能出现在 host 列。 */
  recent: boolean;
}

/**
 * 该 host 节点能否作为「加自定义规则」的目标：非「其它」聚合，且名字像域名/IP（含 `.` 或 `:`）。
 * 后一条挡掉 `host_name_of` 回落到路由规则名（如 `final`）的情形——那不是可加规则的域名。
 */
export function isRuleableHost(node: TopoNode): boolean {
  if (node.type !== 'host' || node.isOthers) return false;
  return node.name.includes('.') || node.name.includes(':');
}

export interface TopoLink {
  /**
   * 缎带身份：`<source>|<target>`（两端节点 id 已含列前缀，故全图唯一）。
   *
   * **不能用数组下标**：links 的顺序继承自 hosts/outbounds 的 count 降序（stats-engine 侧排的），
   * 每来一帧就可能整体重排 —— 下标身份会让 React 把同一个 `<path>` DOM 复用给语义完全不同的缎带
   * （`.hl` 类与 opacity 过渡的中间态一并被继承），也会让 hover 焦点在换帧后静默指向另一条链路。
   * 1s 一拍时这是「偶尔跳一下」，拓扑提频到 4Hz 后就是常态。
   */
  id: string;
  source: string;
  target: string;
  value: number;
  path: string;
  sourceY: number;
  targetY: number;
  heightSource: number;
  heightTarget: number;
}

/* ── 几何：原型 renderSankey（:3515-3517）── */
export const NODE_WIDTH = 8; // 原型 bw
const NODE_GAP = 12; // 原型 gap：同列堆叠间距
const PAD_TOP = 28; // 原型 padT：为列标签留白
const PAD_BOTTOM = 14; // 原型 padB
const COL_X_SOURCE = 0.12; // 原型 xDev（条中心）
const COL_X_HOST = 0.5; // 原型 xDom（条左缘）
const COL_X_OUTBOUND = 0.88; // 原型 xOut（条右缘）

/* ── 缩放：issue #303 题 1B 定稿 + 2026-08-17 大窗口密度复审 ── */
/**
 * 单条流向的视觉粗度上限。
 *
 * 48–56px 在只有 1–3 个目标时仍会把流向画成大色块；36px 已能承载 11.5px 标签和 18px
 * 独立命中区，又不会随窗口最大化继续膨胀。纵向空间只用于增加可见目标槽位，不能拿来加粗已有流向。
 */
export const MAX_FLOW_THICKNESS = 36;
const SOURCE_HEIGHT = MAX_FLOW_THICKNESS; // 左条只是设备锚；与单条流向共用粗度上限
const BAR_HEIGHT_MAX = MAX_FLOW_THICKNESS;
const MID_TOTAL_RATIO = 0.8; // 中列总高上限 = 可用高 × 此比例
const OUT_TOTAL_SINGLE = MAX_FLOW_THICKNESS; // 单出口与设备锚等高
const OUT_TOTAL_MULTI = MAX_FLOW_THICKNESS * 2; // 多出口共用紧凑总预算；单条仍受 BAR_HEIGHT_MAX 约束
const MIN_BAR_HEIGHT = 2; // 条最小视觉高度（可细到 2px，命中区由 hitBox 兜底）
/** 容量节距 = 视觉条/标签最低舒适占用 4px + 12px 固定间距；运行态默认画布实测 301px 仍给出 16 槽。 */
const NODE_SLOT_PITCH = 16;
export const DEFAULT_TOPOLOGY_SLOTS = 16;
/** 与 Rust command 的输入闸一致；超大/多屏窗口也不得制造无界投影。 */
export const MAX_TOPOLOGY_SLOTS = 128;
/** 980×740 下五语种画布高度的上界档；默认密度不因文案换行而漂移。 */
const DEFAULT_CANVAS_HEIGHT_CEILING = 340;

/** 按 SVG 实测高度计算 host/outbound 列的绘制预算；宽度只影响标签，不改变纵向容量。 */
export function topologySlotCapacity(canvasHeight: number): number {
  if (!Number.isFinite(canvasHeight) || canvasHeight <= 0) return DEFAULT_TOPOLOGY_SLOTS;
  const usableWithTrailingGap = canvasHeight - PAD_TOP - PAD_BOTTOM + NODE_GAP;
  const physicalSlots = Math.max(4, Math.floor(usableWithTrailingGap / NODE_SLOT_PITCH));
  if (canvasHeight <= DEFAULT_CANVAS_HEIGHT_CEILING) {
    return Math.min(DEFAULT_TOPOLOGY_SLOTS, physicalSlots);
  }
  return Math.min(
    MAX_TOPOLOGY_SLOTS,
    DEFAULT_TOPOLOGY_SLOTS +
      Math.floor((canvasHeight - DEFAULT_CANVAS_HEIGHT_CEILING) / NODE_SLOT_PITCH),
  );
}

/** 少量高频目标也只长到 36px；连接数决定相对粗细，不得把单条目标拉满整列。 */
function scaledBarHeight(value: number, scale: number): number {
  return Math.min(BAR_HEIGHT_MAX, Math.max(MIN_BAR_HEIGHT, value * scale));
}

/* ── 命中区：与视觉尺寸解耦（issue303 题 2·②）── */
/** 条随连接数反比缩水（50 连接时仅 2.6px），靶子太小右键戳不中 → 命中区不跟随视觉尺寸。 */
const HIT_MIN_HEIGHT = 18;
/** 向标签文字侧的横向延伸：标签 pointer-events:none，不覆盖则点域名会穿透。 */
const HIT_LABEL_REACH = 96;

/* ── 配色：原型 renderSankey（:3524-3528）── */
const COLOR_SOURCE = 'hsl(var(--aurora))';
const COLOR_HOST = 'hsl(var(--flow))';
/** 「其它」聚合组与「直连」出口同走次要灰——原型右列已用 fg-faint 表达「次要/非代理」，此处沿用同一配色语言。 */
const COLOR_MUTED = 'hsl(var(--fg-faint))';
const COLOR_OUTBOUND = 'hsl(var(--flow))';
/**
 * 「阻断」出口用警示色而非错误色。
 *
 * 必须与正常代理可辨：这条腿上的流量是被丢弃的，画成同色就是谎报。
 * 但**不能用 `--err`**（2026-07-30 陈先生裁定「阻断只是出口的一个选择」后订正）：
 *   ① 阻断是用户主动选择，不是故障，红色会读成「出了问题」；
 *   ② 这条腿也承接应用分流的 `action=block`，那类规则是常驻的 ⇒ 拓扑上会常驻红条，
 *      把 `--err`「需要你注意」的信号脱敏掉，代价落在真正的错误上。
 * 也不能复用 `COLOR_MUTED`：直连是「出去了但没走代理」，阻断是「压根没出去」，语义相反。
 */
const COLOR_BLOCKED = 'hsl(var(--warn))';

/** 出口是否走「直连」次要色：sing-box 的 direct outbound tag。 */
function isDirectOutbound(name: string): boolean {
  return name.toLowerCase() === 'direct';
}

/**
 * 出口是否是「阻断」：sing-box 的 block outbound tag。
 *
 * 这条腿既可能来自应用分流的 `action=block`，也可能来自全局出口选阻断（proxy-selector default=block）
 * —— 两者在 stats 帧里都表现为 `block` 这个出站 tag，对拓扑而言同义：**流量到此被丢弃**。
 * 不区分来源是有意的：拓扑画的是实际流向，不是配置意图。
 */
function isBlockedOutbound(name: string): boolean {
  return name.toLowerCase() === 'block';
}

/** 出口条颜色：阻断 > 直连 > 普通代理（阻断优先，因为它是唯一「流量没出去」的语义）。 */
function outboundColor(name: string): string {
  if (isBlockedOutbound(name)) return COLOR_BLOCKED;
  if (isDirectOutbound(name)) return COLOR_MUTED;
  return COLOR_OUTBOUND;
}

/**
 * 节点命中区（相对节点 g 原点）：纵向至少 HIT_MIN_HEIGHT，但不超过「条高 + NODE_GAP - 2」——
 * 恒不与相邻节点命中区重叠（同列间距恒为 NODE_GAP）；横向覆盖条 + 标签文字侧。
 * 防重叠优先于最小高度：2px 条时取 12px，仍是视觉尺寸的 6 倍。
 */
export function hitBox(node: TopoNode): { x: number; y: number; width: number; height: number } {
  const maxH = node.height + NODE_GAP - 2; // 上界：吃掉间距但留 2px，杜绝相邻重叠
  const height = Math.min(Math.max(node.height, HIT_MIN_HEIGHT), Math.max(maxH, node.height));
  const y = (node.height - height) / 2; // 以条为中心纵向扩展
  // 标签在 source/host 左侧、outbound 右侧（见组件里 text 的 x 与 textAnchor）
  const towardLabelLeft = node.type !== 'outbound';
  return {
    x: towardLabelLeft ? -HIT_LABEL_REACH : -10,
    y,
    width: HIT_LABEL_REACH + NODE_WIDTH + 10,
    height,
  };
}

/** 缎带 id：两端节点 id 拼接。节点 id 已含列前缀（`mid-` / `out-`），故 (source,target) 对全图唯一。 */
export function linkId(source: string, target: string): string {
  return `${source}|${target}`;
}

/**
 * 从焦点节点出发收集整条链路上的节点 id + 缎带 id（[`linkId`]）—— hover 与检索共用同一套高亮语义。
 * 沿链路向上游(target→source)与下游(source→target)各做一次 BFS，收敛即停；两端都在链路集内的缎带一并纳入。
 * focusNodes 为空 → 返回空集（调用方据此区分「无命中」与「未激活」）。
 */
export function collectLinkedIds(links: TopoLink[], focusNodes: string[]): Set<string> {
  const set = new Set<string>(focusNodes);
  if (focusNodes.length === 0) return set;

  const walk = (forward: boolean) => {
    const acc = new Set<string>(focusNodes);
    let changed = true;
    while (changed) {
      changed = false;
      links.forEach((l) => {
        const [from, to] = forward ? [l.source, l.target] : [l.target, l.source];
        if (acc.has(from) && !acc.has(to)) {
          acc.add(to);
          changed = true;
        }
      });
    }
    return acc;
  };

  const pathNodes = new Set([...walk(false), ...walk(true)]);
  pathNodes.forEach((id) => set.add(id));
  links.forEach((l) => {
    if (pathNodes.has(l.source) && pathNodes.has(l.target)) set.add(l.id);
  });
  return set;
}

/**
 * 检索匹配：大小写不敏感子串，命中 host 节点名（域名或 IP —— 聚合侧本就是同一字段）与出口节点名。
 * source 节点不参与匹配（它是设备锚，非检索目标）。空 query → 空数组（调用方据此判定未检索）。
 *
 * 后端已先用完整活动表过滤，再按画布容量投影；因此这里的匹配只负责当前投影的视觉高亮。
 */
export function matchNodeIds(nodes: TopoNode[], query: string): string[] {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  return nodes
    .filter((n) => n.type !== 'source' && n.name.toLowerCase().includes(q))
    .map((n) => n.id);
}

/** Sankey 缎带路径：两段三次贝塞尔（顶/底）+ 直线闭合。纯字符串数学（原型 `band()` 同构）。 */
export function getSankeyPath(
  x0: number,
  y0: number,
  x1: number,
  y1: number,
  h0: number,
  h1: number
): string {
  const xi = (x0 + x1) / 2;
  return (
    `M ${x0} ${y0} C ${xi} ${y0}, ${xi} ${y1}, ${x1} ${y1}` +
    ` L ${x1} ${y1 + h1}` +
    ` C ${xi} ${y1 + h1}, ${xi} ${y0 + h0}, ${x0} ${y0 + h0}` +
    ` L ${x0} ${y0} Z`
  );
}

/**
 * 把 stats-engine 已聚合好的连接快照摆成三列 Sankey（source / host / outbound）。
 * 聚合（主要/最近/其它 + 出口容量）已下沉 Rust 侧动态投影 —— 本函数只做坐标布局。
 *
 * @param canvasHeight 画布实测高度（`.sankey` 填满卡片剩余空间，故必须传实测值而非固定值）
 */
export function computeTopologyLayout(
  aggregate: ConnectionsAggregate,
  width: number,
  t: (key: string) => string,
  canvasHeight: number
): { nodes: TopoNode[]; links: TopoLink[] } {
  if (aggregate.hosts.length === 0 || width <= 0 || canvasHeight <= 0) return { nodes: [], links: [] };

  // 「其它」显示名：聚合侧用 sentinel 标记（它不知 i18n）→ 此处替换为本地化文案。
  // isOthers 按 sentinel 判定而非显示名比较，杜绝真实 host 恰为本地化「其它」文案时被误染/撞 id。
  const middle = aggregate.hosts.map((h) => ({
    name: h.name === TOPOLOGY_OTHERS_KEY ? t('home.others') : h.name,
    value: h.count,
    flows: new Map(h.flows.map((f) => [f.outbound, f.count])),
    isOthers: h.name === TOPOLOGY_OTHERS_KEY,
    recent: h.recent,
  }));
  const outbounds = aggregate.outbounds.map((o) => ({
    key: o.name,
    name: o.name === TOPOLOGY_OTHERS_KEY ? t('home.otherOutbounds') : o.name,
    value: o.count,
    isOthers: o.name === TOPOLOGY_OTHERS_KEY,
  }));

  const contentTop = PAD_TOP;
  const available = Math.max(0, canvasHeight - PAD_TOP - PAD_BOTTOM);
  const contentMid = contentTop + available / 2; // 各列组高居中基准（原型 padT/padB 不对称，故非画布中心）

  const totalConnections = middle.reduce((acc, m) => acc + m.value, 0);
  const midGapTotal = Math.max(0, middle.length - 1) * NODE_GAP;
  const outGapTotal = Math.max(0, outbounds.length - 1) * NODE_GAP;

  // 上限是天花板不是目标值：连接少时不超过 BAR_HEIGHT_MAX，多了才被总量上限压下去。
  const maxContentHeight = Math.max(0, available - Math.max(midGapTotal, outGapTotal));
  const midCap = maxContentHeight * MID_TOTAL_RATIO;
  const outCap = outbounds.length === 1 ? OUT_TOTAL_SINGLE : OUT_TOTAL_MULTI;
  const midScale = Math.min(BAR_HEIGHT_MAX, midCap / (totalConnections || 1));
  const outScale = Math.min(BAR_HEIGHT_MAX, outCap / (totalConnections || 1));

  const nodes: TopoNode[] = [];

  /* ── 左列：设备（恒定高度锚）── */
  const sourceNode: TopoNode = {
    id: 'source',
    name: t('home.myDevice'),
    type: 'source',
    value: totalConnections,
    x: width * COL_X_SOURCE - NODE_WIDTH / 2, // 原型 xDev 是条中心
    y: contentMid - SOURCE_HEIGHT / 2,
    height: SOURCE_HEIGHT,
    color: COLOR_SOURCE,
    isOthers: false,
    recent: false,
  };
  nodes.push(sourceNode);

  /* ── 中列：活跃目标 ── */
  const midGroupHeight =
    middle.reduce((acc, m) => acc + scaledBarHeight(m.value, midScale), 0) + midGapTotal;
  let cursor = contentMid - midGroupHeight / 2;
  const midNodes = new Map<string, TopoNode>();
  const hostX = width * COL_X_HOST;

  middle.forEach((m) => {
    const height = scaledBarHeight(m.value, midScale);
    const node: TopoNode = {
      id: `mid-${m.name}`,
      name: m.name,
      type: 'host',
      value: m.value,
      x: hostX,
      y: cursor,
      height,
      color: m.isOthers ? COLOR_MUTED : COLOR_HOST,
      isOthers: m.isOthers,
      recent: m.recent,
    };
    nodes.push(node);
    midNodes.set(m.name, node);
    cursor += height + NODE_GAP;
  });

  /* ── 右列：出站 ── */
  const outGroupHeight =
    outbounds.reduce((acc, o) => acc + scaledBarHeight(o.value, outScale), 0) + outGapTotal;
  cursor = contentMid - outGroupHeight / 2;
  const outNodes = new Map<string, TopoNode>();
  const outCursors = new Map<string, number>();
  const outboundX = width * COL_X_OUTBOUND - NODE_WIDTH; // 原型 xOut 是条右缘

  outbounds.forEach((o) => {
    const height = scaledBarHeight(o.value, outScale);
    const node: TopoNode = {
      id: `out-${o.key}`,
      name: o.name,
      type: 'outbound',
      value: o.value,
      x: outboundX,
      y: cursor,
      height,
      color: o.isOthers ? COLOR_MUTED : outboundColor(o.name),
      isOthers: o.isOthers,
      recent: false,
    };
    nodes.push(node);
    outNodes.set(o.key, node);
    outCursors.set(o.key, cursor);
    cursor += height + NODE_GAP;
  });

  /* ── 缎带：设备 → 域名（左端按 source 内比例分配）── */
  const links: TopoLink[] = [];
  let sourceCursor = sourceNode.y;

  middle.forEach((m) => {
    const midNode = midNodes.get(m.name)!;
    const heightSource = (m.value / (totalConnections || 1)) * sourceNode.height;
    links.push({
      id: linkId(sourceNode.id, midNode.id),
      source: sourceNode.id,
      target: midNode.id,
      value: m.value,
      sourceY: sourceCursor,
      targetY: midNode.y,
      heightSource,
      heightTarget: midNode.height,
      path: getSankeyPath(
        sourceNode.x + NODE_WIDTH,
        sourceCursor,
        midNode.x,
        midNode.y,
        heightSource,
        midNode.height
      ),
    });
    sourceCursor += heightSource;
  });

  /* ── 缎带：域名 → 出站（两端各按本列节点高度的占比算，故三列异 scale 无碍）── */
  middle.forEach((m) => {
    const midNode = midNodes.get(m.name)!;
    let midCursor = midNode.y;

    outbounds.forEach((o) => {
      const flowValue = m.flows.get(o.key);
      if (!flowValue) return;

      const outNode = outNodes.get(o.key)!;
      const heightSource = (flowValue / (m.value || 1)) * midNode.height;
      const heightTarget = (flowValue / (outNode.value || 1)) * outNode.height;
      const outCursor = outCursors.get(o.key)!;

      links.push({
        id: linkId(midNode.id, outNode.id),
        source: midNode.id,
        target: outNode.id,
        value: flowValue,
        sourceY: midCursor,
        targetY: outCursor,
        heightSource,
        heightTarget,
        path: getSankeyPath(
          midNode.x + NODE_WIDTH,
          midCursor,
          outNode.x,
          outCursor,
          heightSource,
          heightTarget
        ),
      });

      midCursor += heightSource;
      outCursors.set(o.key, outCursor + heightTarget);
    });
  });

  return { nodes, links };
}
