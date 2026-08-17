/**
 * 首页渲染预算门 —— 守住「首页每帧 / 每次 store 提交只做必要的那点活」。
 * 起于拓扑（门 1–3），随后并入同类判据：aggregate 只持令牌不挂帧监听（门 4）、
 * 延迟表按出口选单开合条件订阅（门 5）。判据同源，故不另开文件。
 *
 * # 为什么需要一道门
 *
 * 真机实测（陈先生 2026-07-30，活动监视器）：Polaris 总驻留 257.5 MB 里 **208.4 MB（81%）在 WebKit 侧**，
 * 其中 `Polaris Graphics and Media` 独占 73.3 MB / GPU 2.5%。Rust 侧 47.6 MB 反而很健康。
 * 拓扑又刚从 1s 提频到 250ms（`AGGREGATE_POLL_INTERVAL`），任何「每帧白付」的成本都直接 ×4。
 *
 * 本文件钉住的三条不变式都是**接线**性质：它们一旦被顺手改掉，UI 表现**完全不变**（memo 摘掉照样渲染、
 * 隐藏的兜底列表照样看不见），只是悄悄多烧 CPU/GPU —— 也就是说没有任何人工验收路径会发现，
 * 只能靠门。
 *
 * # 本机实测口径（Playwright Chromium 149 + WebKit 26.5，**双引擎数字完全一致**，默认 16 槽满载帧、topo 容器 1052px）
 *
 * | 指标 | 加固前 | 加固后 |
 * |---|---:|---:|
 * | `#topo-card` 后代元素 | 225 | 144 |
 * | 其中隐藏兜底列表子树 | 81（恒 `display:none`） | 0 |
 * | 无关父态变化 60 次 → 子树渲染次数 | 60 | **0** |
 * | 40 个 aggregate 帧期间的 DOM attribute/characterData 变更 | 5414 / 640 | 4814 / **40** |
 *
 * 即：宽屏下每秒有 120 次 DOM 写入落在一个用户永远看不见的子树上，现在是 0。
 * **MB 数本机量不到**（Linux/WebKitGTK ≠ macOS/WKWebView，进程模型也不同）——本门只守结构性指标。
 *
 * # 为什么第 2、3 条是源码结构断言
 *
 * 本仓 vitest 是 `environment:'node'`（`vite.config.ts`），无 jsdom、无 CSSOM，**有意为之**
 * ⇒ 渲染不了组件，「宽屏下那 81 个节点还在不在」在这一层根本不可观测。但「源码里那个守卫还在不在」
 * 是纯文本可断言的，且正是缺陷复发的那一层 —— 与 `lib/tooltip-wiring.test.ts` 同一判据来源。
 * 第 1 条则是真断言（memo 是运行时可观测的对象形态），不必退到文本。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createTopicSubscription } from '@/lib/topic-subscription';
import { EMPTY_LATENCY_MAP, latencyMapWhen } from '@/store/use-latency-store';

/** `@/i18n` 在模块加载期就写 `<html dir/lang>`（i18n/index.ts:81），node 下无 document ⇒ 先垫桩。
 *  与 `harness-screens.test.tsx` 同款，别为它引 jsdom。 */
(globalThis as unknown as { document: unknown }).document = {
  documentElement: { dir: '', lang: '', getAttribute: () => null, setAttribute: () => {} },
  body: { nodeType: 1 },
};

/** 顶层 await 而非 `it()` 内 `await import()`：这条 import 链带上 i18n 全量 locale + ipc + 各 store，
 *  全量跑时 transform 竞争下轻松超 vitest 默认 5s testTimeout（实测会假红）。收集期不受该超时约束，
 *  与 `harness-screens.test.tsx` 的顶层 `await import` 同款。 */
const { ConnectionTopology } = await import('./ConnectionTopology');

const HERE = fileURLToPath(new URL('.', import.meta.url));
const TOPO_SRC = readFileSync(`${HERE}ConnectionTopology.tsx`, 'utf8');
const HOME_SRC = readFileSync(`${HERE}HomeScreen.tsx`, 'utf8');

/** ── 自曝：扫描面塌了（文件改名/结构大改）必须当场炸，而不是让下面的断言在空文本上恒真 ── */
describe('拓扑渲染预算门 · 扫描面自曝', () => {
  it('两个被扫文件都读到了且含预期锚点', () => {
    expect(TOPO_SRC).toContain('className="sankey-fallback"');
    expect(TOPO_SRC).toContain('new ResizeObserver');
    expect(HOME_SRC).toContain('<ConnectionTopology');
  });
});

describe('门 1 · ConnectionTopology 必须是 memo 组件', () => {
  it('导出的是 React.memo 包装体，不是裸函数', () => {
    // memo 包装体是个对象且带 react.memo 标记；裸函数 typeof === 'function' 且无 $$typeof。
    expect(typeof ConnectionTopology).toBe('object');
    expect((ConnectionTopology as unknown as { $$typeof: symbol }).$$typeof).toBe(
      Symbol.for('react.memo')
    );
  });
});

describe('门 2 · memo 的前提：调用点两个 props 都必须引用稳定', () => {
  /**
   * memo 只在 props 浅比较相等时才 bail-out。调用点一旦塞进内联对象 / 数组 / 箭头函数，
   * 每次父渲染都会造新引用 ⇒ memo 恒失效，**比不加还慢**（白多一层比较）。
   * 故这里要求每个 prop 的表达式都是「裸标识符 / 成员访问 / 单个前缀 `!`」这类零分配形式。
   */
  const STABLE_EXPR = /^!?[A-Za-z_$][\w$]*(\.[A-Za-z_$][\w$]*)*$/;

  it('<ConnectionTopology> 的每个 prop 都是零分配表达式', () => {
    const open = HOME_SRC.indexOf('<ConnectionTopology');
    const tag = HOME_SRC.slice(open, HOME_SRC.indexOf('/>', open) + 2);
    const props = [...tag.matchAll(/(\w+)=\{([^}]*)\}/g)].map(([, name, expr]) => [
      name,
      expr.trim(),
    ]);
    // 正向对照：props 一个都没解析出来时上面的断言恒真，故先钉住唯一入口 prop。
    expect(props.map(([n]) => n)).toEqual(['disconnected']);
    for (const [name, expr] of props) {
      expect(`${name}=${expr}`).toMatch(new RegExp(`^${name}=${STABLE_EXPR.source.slice(1, -1)}$`));
    }
  });
});

describe('门 3 · 窄屏兜底列表不许无条件渲染', () => {
  /** 注释里合法地出现 `size.w === 0` 等字样，扫描前先剥掉，免得守卫被删了仍靠注释蒙混过关。 */
  const CODE = TOPO_SRC.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');

  it('`.sankey-fallback` 紧邻的上文是「量到的尺寸为 0」守卫', () => {
    const at = CODE.indexOf('className="sankey-fallback"');
    expect(at).toBeGreaterThan(0);
    const before = CODE.slice(0, at);
    // 期望形如：`{!disconnected && size.w === 0 && (\n  <div `
    expect(before).toMatch(/\{[^{}]*size\.w === 0[^{}]*&&\s*\(\s*<div\s*$/);
  });

  it('守卫复用既有的那一个 ResizeObserver —— 不许为它新引第二个', () => {
    // 「省 80 个隐藏节点」若要拿一个新 observer + 重排监听去换，未必是净收益；
    // 判据成立的前提正是「零新增观测」，故把 observer 数量一并钉死。
    expect(CODE.match(/new ResizeObserver/g)?.length).toBe(1);
  });
});

/**
 * 门 4 · 首页只持 **topology 令牌**，不挂帧监听、也不订 aggregate。
 *
 * Tauri 只在「该 webview 对该事件有 JS 监听」时才向本窗口发一段 eval 脚本。首页原先给
 * aggregate 挂了一条**回调体为空**的监听 —— 于是整条 eval 链每帧真实发生一遍：一份 UTF-16
 * 源码字符串 + 一次 JSC parse/bytecode 分配（源码逐帧不同 ⇒ code cache 恒不命中）+ 一份 JS
 * 对象图，随即全成垃圾。250ms 闸门下就是每秒最多 4 次白付，而首页没有任何读点消费这些帧。
 *
 * topic 也必须是 `topology` 而不是 `aggregate`：后者是排名页要的 Top-N 聚合载荷，后端每次 emit 要
 * 在完整活动表上做一次 O(n log n) 聚合 + 载荷序列化 + 跨进程搬运。首页订它 = 首页在场时这份功永远
 * 白做（首页拿到信号后自己拉有界投影，从不读那份载荷）。
 *
 * 但 `subscribe`/`unsubscribe` **必须留着**：四条 topic 共用一条 gRPC 连接流，后端按
 * `should_stream_connections()`（aggregate ∪ topology ∪ detail ∪ closed）判需求，全零才停流；
 * 撤掉令牌会在首页可见时误停整条流、拓扑冻结。令牌与数据帧是两件事，本门把两件事分别钉住。
 */
describe('门 4 · 首页只持 topology 令牌，不挂帧监听', () => {
  const CODE = HOME_SRC.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/^\s*\/\/.*$/gm, '');

  it('首页**不得**注册 aggregate 帧监听（空回调 = 每帧白付一条 eval 链）', () => {
    // 变异对照：把 `onFrame: () => () => {}` 改回 `onFrame: (cb) => api.stats.onConnectionsAggregate(cb)`
    // ⇒ 本条转红。注释里逐字写着旧形态，故必须扫去注释后的源码。
    expect(CODE, '首页又挂回 aggregate 帧监听了').not.toContain('onConnectionsAggregate');
    expect(CODE).toMatch(/onFrame:\s*\(\)\s*=>\s*\(\)\s*=>\s*\{\}/);
  });

  it('令牌是 topology（订成 aggregate = 首页在场时白付整表聚合）', () => {
    expect(CODE).toContain("subscribe: () => api.stats.subscribe('topology')");
    expect(CODE).toContain("unsubscribe: () => api.stats.unsubscribe('topology')");
    // 反向对照：首页**一处**都不得再出现 aggregate 令牌（订上就把后端那条门重新顶开）。
    expect(CODE, '首页又订回 aggregate 了').not.toContain("subscribe('aggregate')");
  });

  /**
   * 「撤掉首页帧监听」这件事**安全**的两个前提，本条只证第一个：
   *  (a) `createTopicSubscription` 不吞帧、`dispose()` 只摘自己那条监听 —— **本条**（端口契约直测）；
   *  (b) 连接页**确实**还挂着 aggregate 真监听 —— 不在本文件，由
   *      `lib/topic-subscription-wiring.test.ts` 的「真帧监听的分布」正向对照钉住
   *      （把连接页端口改成 `onFrame: () => () => {}` ⇒ 那条转红）。
   *
   * 分工的理由：这里原先跨文件逐字断言 `ConnectionsScreen.tsx` 的 `onFrame:` 那一行，连接页任何
   * 等价重构都会让**首页**这道门转红、红点落在无关文件上。(b) 属于「谁挂着监听」这类全局接线事实，
   * 归 wiring 那份统一记账；本文件只留可直测的 (a)，与既有分层一致（真断言优先于源码文本）。
   */
  it('端口契约：帧只发给挂了监听的那条，dispose 只摘自己（首页空壳监听不吞别人的帧）', () => {
    const listeners = new Set<(d: number) => void>();
    const tokens: string[] = [];
    const emit = (d: number) => listeners.forEach((cb) => cb(d));
    /** 同一条后端流的两个订阅端口；`attachFrames=false` 就是首页那条（回调体为空的监听）。 */
    const port = (attachFrames: boolean) => ({
      onFrame: (cb: (d: number) => void) => {
        if (!attachFrames) return () => {};
        listeners.add(cb);
        return () => {
          listeners.delete(cb);
        };
      },
      subscribe: async () => {
        tokens.push('sub');
      },
      unsubscribe: async () => {
        tokens.push('unsub');
      },
    });

    const got: number[] = [];
    const home = createTopicSubscription<number>(port(false), () => {
      throw new Error('首页那条不挂监听，不该收到任何帧');
    });
    const ranking = createTopicSubscription<number>(port(true), (d) => got.push(d));
    home.setWanted(true);
    ranking.setWanted(true);
    emit(1);
    emit(2);
    // 两条订阅令牌都发了：后端按 aggregate ∪ detail ∪ closed 判需求，首页撤监听不撤令牌。
    expect(tokens).toEqual(['sub', 'sub']);
    expect(got).toEqual([1, 2]);
    // 首页退订/卸载后，排名页照收 —— 这就是「撤首页那条不连累它」的直接证据。
    home.dispose();
    emit(3);
    expect(got).toEqual([1, 2, 3]);
    ranking.dispose();
    emit(4);
    expect(got).toEqual([1, 2, 3]);
  });
});

/**
 * 门 5 · 首页的延迟表按出口选单开合**条件订阅**。
 *
 * 被守的缺陷与门 4 同型（每帧/每次提交白付），只是发生在延迟流上：`latencies` 全文唯一去处是
 * `<NodeMenu latencies={latencies}>`，而 `NodeMenu` 在 `open` 为假时第一行就
 * `return <div hidden />`。无条件订整表 ⇒ 停在首页点「全部测速」测 200 个节点 = 200 次 store 提交
 * = 首页整棵子树重渲 200 次，而屏幕上没有任何一处在显示延迟。
 *
 * 放本文件而不是 `store/latency-wiring-invariants.test.ts`：那份守的是**状态归属**（真值在 store
 * 还是组件私有 useState、订阅挂哪一层），它的两组 `it.each(CONSUMERS)` 对「订不订」一视同仁；
 * 本条守的是**订阅时机**（条件 vs 无条件），判据来源是渲染预算，与节点页的对应门
 * （`nodes/nodes-render-budget.test.tsx` 门 2）对称。混进那边会让 CONSUMERS 那两组的语义漂掉。
 */
describe('门 5 · 首页延迟表按选单开合条件订阅', () => {
  const CODE = HOME_SRC.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/^\s*\/\/.*$/gm, '');

  it('订阅是**条件式**的，不得退回无条件订整表', () => {
    // 变异对照：改回 `useLatencyStore((s) => s.latencyMap)` ⇒ 本条转红。
    // 注释里逐字写着旧形态与「勿改回」，故必须扫去注释后的源码。
    expect(CODE, '首页又无条件订整张延迟表了 ⇒ 每次逐节点回包都重渲整棵子树').not.toMatch(
      /useLatencyStore\(\s*\(s\)\s*=>\s*s\.latencyMap\s*\)/
    );
    expect(CODE).toContain('useLatencyStore(latencyMapWhen(nodeMenuOpen))');
  });

  it('关闭档返回**模块级冻结哨兵**（写成 `{}` 字面量 ⇒ 每次提交仍判不等 ⇒ 改了等于白改）', () => {
    const sel = latencyMapWhen(false);
    expect(sel({ latencyMap: { a: 1 } })).toBe(sel({ latencyMap: { b: 2 } }));
    expect(sel({ latencyMap: { a: 1 } })).toBe(EMPTY_LATENCY_MAP);
    expect(Object.isFrozen(EMPTY_LATENCY_MAP)).toBe(true);
    expect(Object.keys(EMPTY_LATENCY_MAP)).toEqual([]);
  });

  it('打开档拿的是 store 那张表**本体** —— 选单打开那一次就读到当下真值，不是旧快照', () => {
    const state = { latencyMap: { a: 1 } };
    expect(latencyMapWhen(true)(state)).toBe(state.latencyMap);
    // 选择器每次渲染都会被重建（调用点写在渲染里），稳定性只能来自哨兵引用，不能来自闭包缓存。
    expect(latencyMapWhen(false)).not.toBe(latencyMapWhen(false));
  });
});

describe('准确性门 · 完整活动表投影与绘制预算解耦', () => {
  it('常态和搜索都等待完整表变化监听就绪，再走后端按槽位投影', () => {
    const ready = TOPO_SRC.indexOf('onConnectionsTopologyChangedReady');
    const project = TOPO_SRC.indexOf('projectTopology(normalizedSearch, projectionSlots)');
    expect(ready).toBeGreaterThan(0);
    expect(project).toBeGreaterThan(0);
    expect(TOPO_SRC).toContain('projection?.aggregate');
    expect(TOPO_SRC).toContain('topologySlotCapacity(size.h)');
    expect(TOPO_SRC).toContain("t('home.connectionFlow')");
  });

  it('当前查询和槽位的回包抵达前不得用旧投影猜测“无命中”', () => {
    expect(TOPO_SRC).toContain(
      'searching && projectionResolved && displayAggregate?.total === 0'
    );
  });
});
