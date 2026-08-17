/**
 * 节点屏渲染预算门 —— 守住「测速逐节点回包时，只有回包的那一张卡重渲」以及「分批只切渲染尾」。
 *
 * # 被守的缺陷长什么样
 *
 * 原状三处叠在一起：父层 `useLatencyStore((s) => s.latencyMap)` **无条件**订整张延迟表；
 * `visibleServers` 的 `useMemo` 依赖里带着这张表；`NodeCard` 全文件零 `memo`。于是一轮测速
 * （200 个节点 = 200 次 store 提交）会触发 200 次父层重渲 → 200 次整表 filter+sort →
 * 200 × N 张卡重建 VDOM，每张卡 ≥22 个 DOM 元素含 10 处内联 svg。
 *
 * 这类缺陷的共同点是**改对了和没改，UI 表现完全一样**（memo 摘掉照样渲染、订阅放宽照样显示正确
 * 数值），没有任何人工验收路径会发现，只能靠门 —— 判据来源同 `home/topology-render-budget.test.ts`。
 *
 * # 两条硬不变量（本文件的存在理由；违反即回归，内存数字更低也不算）
 *
 * ① **及时性**：任一节点延迟回包，必须在同一次 store 提交后的下一次 React commit 反映到该节点
 *    卡片；按延迟排序时顺序同步更新；**不得**新增 timer / 节流 / 合批。
 * ② **检索真值**：分批只切**渲染尾**。`visibleServers` 已是 search / protoFilter 作用后的完整
 *    结果，全选 / 工具栏「测速」（可见集）/ 批选条三处必须继续读它，不得读切片后的数组。
 *    反面教训是日志页「500 行以外搜不到」那类回归。
 *
 * # 手段分层（哪条能真断言就不退到源码文本）
 *
 *  - **真断言**：store 通知的同步性、选择器的引用稳定性、比较器的重排结果、`memo` 包装体形态、
 *    以及用 `react-dom/server` 真渲染出来的首帧卡片数（本仓既有先例：`harness-screens.test.tsx`、
 *    `initial-tab-first-frame.test.tsx`，node 环境 + `renderToStaticMarkup`，**不引 jsdom**）。
 *  - **源码结构门**：只用于「接线在不在」这类在 node 环境不可观测、却正是缺陷复发那一层的事实
 *    （同 `nodes-speedtest-wiring.test.ts` / `lib/tooltip-wiring.test.ts` 的判据来源）。
 */
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ServerConfig, UserConfig } from '@/contracts/types';
import { DEMO_CONFIG } from '../../../../harness-fixture';
import { SCROLL_BATCH_PAGE } from '@/lib/use-scroll-batch';
import { sortServersByLatency } from '@/domain/server-latency-sort';
import { speedTestableIds } from '@/domain/endpoint-routes';
import { EMPTY_LATENCY_MAP, latencySortSelector } from './nodes-logic';
import { useLatencyStore } from '@/store/use-latency-store';

/** t() 桩：返回 key 本身（同 `initial-tab-first-frame.test.tsx`）——断言落在结构上，与语种解耦。 */
vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: 'zh-CN' } }),
}));

/** node 无 document，而部分模块加载期就要写 `<html dir/lang>` / portal 到 body。 */
(globalThis as unknown as { document: unknown }).document = {
  documentElement: { dir: '', lang: '', getAttribute: () => null, setAttribute: () => {} },
  body: { nodeType: 1 },
};

const read = (rel: string): string =>
  readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

/**
 * 去注释后的源码 —— **负向断言全部跑在它上面**。本仓注释习惯逐字引用被替换掉的旧形态
 * （本文件相关的就有「勿把 latencies 改回 useState」「别改回 `(s) => s.latencyMap`」），
 * 直接扫原文会被自己的说明文字骗红/骗绿。`[^:]` 前瞻避免把 `https://` 当行注释切掉。
 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');

const NODES_RAW = read('./NodesScreen.tsx');
const CARD_RAW = read('./NodeCard.tsx');
const MENU_RAW = read('../home/NodeMenu.tsx');
const SHELL_RAW = read('../../layout/AppShell.tsx');
const NODES = code(NODES_RAW);
const CARD = code(CARD_RAW);
const MENU = code(MENU_RAW);

/** 取顶层 `const <name> = useCallback(` 到其收尾 `);`（列 2 缩进）的函数体（同 nodes-speedtest-wiring）。 */
function callbackBody(src: string, name: string): string {
  const anchor = `const ${name} = useCallback(`;
  const start = src.indexOf(anchor);
  expect(start, `锚点消失，守卫已失去判据: ${anchor}`).toBeGreaterThan(-1);
  const rest = src.slice(start);
  const end = rest.indexOf('\n  );');
  expect(end, `找不到 ${name} 的 useCallback 收尾`).toBeGreaterThan(-1);
  return rest.slice(0, end);
}

const occurrences = (src: string, needle: string): number => src.split(needle).length - 1;

/* ════════════════════════════════════════════════════════════════════════════
 * 自曝：扫描面塌了（文件改名 / 结构大改 / 读空）必须当场炸，而不是让下面的断言在空文本上恒真
 * ════════════════════════════════════════════════════════════════════════════ */

describe('自曝 · 扫描面还在', () => {
  it('四个被扫文件都读到了且是本屏的文件', () => {
    expect(NODES_RAW.length).toBeGreaterThan(1000);
    expect(CARD_RAW.length).toBeGreaterThan(1000);
    expect(MENU_RAW.length).toBeGreaterThan(1000);
    expect(NODES).toContain('export function NodesScreen');
    expect(CARD).toContain('function NodeCardView');
    expect(MENU).toContain('export function NodeMenu');
  });

  it('去注释后仍是可断言的代码（防 code() 把源码整段吃掉 → 负向断言恒绿）', () => {
    expect(NODES.length).toBeGreaterThan(NODES_RAW.length / 3);
    expect(CARD.length).toBeGreaterThan(CARD_RAW.length / 3);
    expect(NODES).not.toContain('1:1 提取自原型');
  });

  /**
   * 分批的滚动监听靠 `gridRef.current.closest('.main-scroll')` 找 `AppShell` 的滚动容器
   * （本屏自己不滚）。**本条的射程只有一个向量：那个类名被改掉。**
   * 另一个向量（网格在运行期不是 `.main-scroll` 的后代 —— 换层级 / 本屏被别处复用 / 渲染进 portal）
   * 两个类名都还在、本条照绿，只有跑起来才看得见 —— 那一路由代码自身 fail-open 兜底（下一条钉住）
   * 加一条运行期 `console.warn` 自曝，不再由这道门负责。
   */
  it('AppShell 的主滚动容器仍叫 `.main-scroll`（分批监听的挂载点）', () => {
    expect(SHELL_RAW).toContain('className="main-scroll"');
    expect(NODES).toContain("closest<HTMLElement>('.main-scroll')");
  });

  /**
   * 失效方向必须朝「开」。原实现找不到祖先就直接 `return` ⇒ 永远只剩首批，而用户没有滚动条、
   * 没有任何「还有更多」的暗示，看不出少了 —— 这正是本文件不变量②要防的那种「界面与操作对象脱节」
   * （全选/测速仍读 300，屏幕上只有 60）。宁可一次全渲染。
   */
  it('找不到滚动祖先时 fail-open（一次取到底）+ 运行期自曝，而不是静默只剩首批', () => {
    expect(NODES, '兜底腿没了：closest 落空会退回「永远只有首批」').toContain(
      'renderAllRef.current()'
    );
    expect(NODES, '运行期自曝没了：这一向量在源码门下不可见').toContain('warnMissingScroller()');
  });

  /**
   * fail-open 必须跑在 **passive** 档。它把 count 顶到 total，几千张卡（每卡 ≥22 个 DOM 元素、
   * 含 10 处内联 SVG）若压在 paint 之前，「少显示节点」就被换成「窗口冻住数秒」——用户连
   * 首批都看不到，比原缺陷更糟。判据：`renderAllRef.current()` 所在的 effect 不是 layout 档。
   */
  it('fail-open 走 passive 档（layout 档下一次全渲染 = paint 前冻住）', () => {
    const at = NODES.indexOf('renderAllRef.current()');
    expect(at, 'fail-open 腿不见了').toBeGreaterThan(-1);
    const openedAt = NODES.lastIndexOf('Effect(', at);
    expect(NODES.slice(openedAt - 24, openedAt + 7)).toContain('useEffect(');
    expect(
      NODES.slice(openedAt - 24, openedAt + 7),
      'fail-open 被挪进 layout 档了 ⇒ 一次全渲染压在 paint 之前'
    ).not.toContain('useIsomorphicLayoutEffect(');
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 不变量① 及时性 —— 回包必须立刻到达那张卡，且不许靠 timer / 节流 / 合批把它推迟
 * ════════════════════════════════════════════════════════════════════════════ */

describe('不变量① · 延迟回包的及时性', () => {
  it('store 提交是**同步**通知的：回包写入即可读，中间没有异步跳板', () => {
    useLatencyStore.setState({ latencyMap: {}, testedAt: {} });
    const seen: (number | null | undefined)[] = [];
    const unsub = useLatencyStore.subscribe((s) => seen.push(s.latencyMap['n1']));
    useLatencyStore.getState().applyLatencyResult('n1', 42);
    // 同一个 tick 里就已经通知过了 —— 若有人在写入路径上加 setTimeout/节流/合批，这里恒为空。
    expect(seen).toEqual([42]);
    expect(useLatencyStore.getState().latencyMap.n1).toBe(42);
    unsub();
  });

  it('每卡按 id 订阅的那格：本节点回包变、别的节点恒等（`Object.is` 稳定 ⇒ 只重渲一张卡）', () => {
    useLatencyStore.setState({ latencyMap: {}, testedAt: {} });
    // 这两条正是 `NodeCard` 里 `(s) => s.latencyMap[server.id]` 的取值语义。
    const readOf = (id: string) => useLatencyStore.getState().latencyMap[id];
    useLatencyStore.getState().applyLatencyResult('n1', 100);
    const otherBefore = readOf('n2');
    useLatencyStore.getState().applyLatencyResult('n1', 55);
    expect(readOf('n1')).toBe(55); // 本卡：值变了 ⇒ 该卡重渲
    expect(readOf('n2')).toBe(otherBefore); // 邻卡：判等 ⇒ 不重渲
    expect(Object.is(readOf('n2'), otherBefore)).toBe(true);
  });

  it('按延迟排序时顺序**同步**更新（比较器读的就是当下这份快照）', () => {
    useLatencyStore.setState({ latencyMap: {}, testedAt: {} });
    const list = [
      { id: 'a', name: 'A' },
      { id: 'b', name: 'B' },
      { id: 'c', name: 'C' },
    ];
    const order = () =>
      sortServersByLatency(list, (id) => useLatencyStore.getState().latencyMap[id], 'asc').map(
        (s) => s.id
      );
    useLatencyStore.getState().applyLatencyResults({ a: 300, b: 200, c: 100 });
    expect(order()).toEqual(['c', 'b', 'a']);
    // 单个节点回包 → 下一次求值立刻反映新顺序，不等任何计时器。
    useLatencyStore.getState().applyLatencyResult('a', 10);
    expect(order()).toEqual(['a', 'c', 'b']);
  });

  it('两个渲染文件里**没有** timer / 节流 / 合批（推迟一拍就违反本不变量）', () => {
    const BANNED = /setTimeout|setInterval|requestAnimationFrame|queueMicrotask|debounce|throttle/;
    // 变异对照：在 NodesScreen 里给延迟加一层 16ms 合批 ⇒ 本条转红。
    expect(NODES, 'NodesScreen 出现了延迟推迟机制').not.toMatch(BANNED);
    expect(CARD, 'NodeCard 出现了延迟推迟机制').not.toMatch(BANNED);
  });

  it('单卡按**自身 id** 细粒度订阅，不是整表灌 prop（否则每次回包要过父层整表重渲）', () => {
    expect(CARD).toMatch(/useLatencyStore\(\s*\(s\)\s*=>\s*s\.latencyMap\[server\.id\]\s*\)/);
    // 原状形态：卡片零订阅、由 NodesScreen `latencyMs={latencies[server.id]}` 灌下来。
    expect(NODES, '调用点又把整表的一格灌回 prop 了').not.toMatch(/latencyMs=\{/);
    expect(CARD, 'NodeCard 退回订整张表 ⇒ 任一节点回包会让全部卡片重渲').not.toMatch(
      /useLatencyStore\(\s*\(s\)\s*=>\s*s\.latencyMap\s*\)/
    );
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 门 1 · NodeCard 必须是 memo 组件，且调用点不得每次渲染都造新引用
 * ════════════════════════════════════════════════════════════════════════════ */

describe('门 1 · NodeCard = memo + 调用点零分配', () => {
  it('导出的是 React.memo 包装体，不是裸函数', async () => {
    const { NodeCard } = await import('./NodeCard');
    // memo 包装体是对象且带 react.memo 标记；裸函数 typeof === 'function' 且无 $$typeof。
    expect(typeof NodeCard).toBe('object');
    expect((NodeCard as unknown as { $$typeof: symbol }).$$typeof).toBe(Symbol.for('react.memo'));
  });

  /**
   * memo 只在 props 浅比较相等时 bail-out。调用点一旦塞进内联箭头 / 内联对象 / 内联数组，
   * 每次父渲染都造新引用 ⇒ memo 恒失效，**比不加还慢**（白多一层比较）。
   * 布尔 / 字符串这类原始值表达式不在此列（它们按值判等，写成什么形状都稳定）。
   */
  const cardTag = (() => {
    const open = NODES.indexOf('<NodeCard');
    expect(open, '调用点消失，本节全部失去判据').toBeGreaterThan(-1);
    return NODES.slice(open, NODES.indexOf('/>', open) + 2);
  })();

  it('调用点不含内联箭头 / 内联对象 / 内联数组', () => {
    // 变异对照：把 `onEdit={editNode}` 改回 `onEdit={(s) => openDialog(editDialogFor(s))}` ⇒ 转红。
    expect(cardTag, '内联箭头会让每张卡的 memo 恒失效').not.toContain('=>');
    expect(cardTag, '内联对象字面量同理').not.toContain('={{');
    expect(cardTag, '内联数组字面量同理').not.toContain('={[');
  });

  it('七个回调 prop 都是**裸标识符**（即 useCallback 出来的稳定引用）', () => {
    const handlers = [...cardTag.matchAll(/\son([A-Z]\w*)=\{([^}]*)\}/g)].map(([, n, v]) => [
      `on${n}`,
      v.trim(),
    ]);
    // 正向对照：一个都没解析出来时下面的 for 恒真，故先钉住条数与名字。
    expect(handlers.map(([n]) => n).sort()).toEqual([
      'onClone',
      'onCopy',
      'onDelete',
      'onEdit',
      'onSpeedTest',
      'onToggleSelect',
      'onUse',
    ]);
    for (const [name, expr] of handlers) {
      expect(`${name}=${expr}`).toMatch(new RegExp(`^${name}=[A-Za-z_$][\\w$]*$`));
    }
  });

  it('`shadowedCidrs` 走一次算好的索引，不在 map 里就地造新数组', () => {
    // 就地 `shadowedIndex.get(id)?.map(...)`（原状）每次父渲染都造新数组 ⇒ 冲突节点的卡恒不 bail-out。
    expect(NODES).toContain('const shadowed = shadowedNamed.get(server.id);');
    expect(NODES).toMatch(/const shadowedNamed = useMemo\(/);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 门 2 · 父层条件订阅 + **模块级稳定哨兵**（这一条塌了，整个改动等于白做）
 * ════════════════════════════════════════════════════════════════════════════ */

describe('门 2 · 非延迟排序时选择器返回稳定引用', () => {
  it('同一档位、不同 state：返回的是**同一个**对象引用', () => {
    // zustand 默认按 Object.is 比较选择器结果来决定要不要重渲。写成 `: {}` 字面量每次都是新对象，
    // 父层照样每次 store 提交重渲 —— 与不做条件订阅逐字等价。本条就是钉这一点。
    const sel = latencySortSelector(false);
    expect(sel({ latencyMap: { a: 1 } })).toBe(sel({ latencyMap: { b: 2 } }));
    expect(sel({ latencyMap: { a: 1 } })).toBe(EMPTY_LATENCY_MAP);
  });

  it('选择器**每次渲染都会被重建**，跨实例也必须返回同一引用', () => {
    // 调用点写的是 `useLatencyStore(latencySortSelector(sortKey === 'lat'))` —— 每渲染一次就是一个
    // 新的选择器函数。稳定性只能来自模块级常量，不能来自 useMemo/闭包缓存。
    const a = latencySortSelector(false);
    const b = latencySortSelector(false);
    expect(a).not.toBe(b);
    expect(a({ latencyMap: { x: 1 } })).toBe(b({ latencyMap: { y: 2 } }));
  });

  it('按延迟排序档：返回的是 store 里那张表**本体**（否则排序读不到新值）', () => {
    const state = { latencyMap: { a: 1 } };
    expect(latencySortSelector(true)(state)).toBe(state.latencyMap);
  });

  it('哨兵是冻结的空表（被误写一格就等于给排序喂了假数据）', () => {
    expect(Object.isFrozen(EMPTY_LATENCY_MAP)).toBe(true);
    expect(Object.keys(EMPTY_LATENCY_MAP)).toEqual([]);
  });

  it('父层确实按 sortKey 条件订阅，且不得退回无条件订整表', () => {
    expect(NODES).toContain("useLatencyStore(latencySortSelector(sortKey === 'lat'))");
    // 原状形态：`const latencies = useLatencyStore((s) => s.latencyMap);`
    expect(NODES, '父层又无条件订整张延迟表了').not.toMatch(
      /useLatencyStore\(\s*\(s\)\s*=>\s*s\.latencyMap\s*\)/
    );
  });

  it('三条动作腿在**点击当刻**取最新快照，不吃闭包里的陈旧表', () => {
    // 父层不再常订整表 ⇒ 闭包里捕获的会是上一次渲染时的旧值：用户刚测完速就删节点，
    // 兜底出口会选到过期的「最快」。故三条腿一律 getState() 现取。
    for (const leg of ['requestDelete', 'deleteBatch'] as const) {
      expect(callbackBody(NODES, leg), `${leg} 没有现取延迟快照`).toContain(
        'useLatencyStore.getState().latencyMap'
      );
    }
    expect(callbackBody(NODES, 'removeWarpNode')).toContain(
      'useLatencyStore.getState().latencyMap'
    );
    expect(occurrences(NODES, 'useLatencyStore.getState().latencyMap')).toBe(3);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 不变量② 检索真值 —— 分批只切渲染尾
 * ════════════════════════════════════════════════════════════════════════════ */

describe('不变量② · 分批只切渲染尾，检索/全选/测速仍对全集生效', () => {
  it('全选读的是 `visibleServers`（过滤后总数），不是切片', () => {
    const body = callbackBody(NODES, 'selectAll');
    expect(body).toContain('visibleServers.map((s) => s.id)');
    expect(body, '全选读到切片了 ⇒ 「全选」只选得到屏幕上画出来的那批').not.toContain(
      'renderedServers'
    );
  });

  it('工具栏「测速」（可见集）读的是 `visibleServers`，不是切片', () => {
    const body = callbackBody(NODES, 'testVisible');
    expect(body).toContain('speedTestableIds(visibleServers');
    expect(body).not.toContain('renderedServers');
  });

  it('批选条（全选复选框 / 批选测速 / 批量复制）三处也都读完整集', () => {
    expect(NODES).toMatch(/aria-checked=\{selectedIds\.size === visibleServers\.length/);
    expect(callbackBody(NODES, 'testSelected')).toContain('speedTestIdsForSelection(');
    expect(callbackBody(NODES, 'testSelected')).toContain('visibleServers');
    expect(callbackBody(NODES, 'copyLinksBatch')).toContain(
      'visibleServers.filter((s) => selectedIds.has(s.id))'
    );
  });

  /**
   * 最强的一条：切片变量在**去注释后的源码里只出现两次** —— 声明处 + `.map()` 处。
   * 任何人把它回灌给全选 / 测可见 / 批选，计数立刻变 3，本条转红。
   * （反面教训：日志页「500 行以外搜不到」正是把渲染切片当成了数据全集。）
   */
  it('切片变量只出现在「声明」与「渲染」两处', () => {
    expect(occurrences(NODES, 'renderedServers')).toBe(2);
    expect(NODES).toContain('const renderedServers = useMemo(');
    expect(NODES).toContain('renderedServers.map((server) => {');
  });

  it('正向对照：读全集与读切片的结果**确实不同**（否则上面几条是空断言）', () => {
    const many = makeServers(SCROLL_BATCH_PAGE * 2 + 7);
    const caps = { mainCorePool: true };
    expect(speedTestableIds(many, caps).length).toBe(many.length);
    expect(speedTestableIds(many.slice(0, SCROLL_BATCH_PAGE), caps).length).toBe(
      SCROLL_BATCH_PAGE
    );
    expect(speedTestableIds(many, caps).length).toBeGreaterThan(SCROLL_BATCH_PAGE);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 门 3 · 分批本身：初批必须覆盖视口，且复用仓内唯一实现
 * ════════════════════════════════════════════════════════════════════════════ */

describe('门 3 · 分批接线', () => {
  it('复用 `lib/use-scroll-batch` 唯一实现，不自持第二份分批常量', () => {
    expect(NODES).toContain('useScrollBatch(');
    expect(NODES, '又抄了一份内联分批实现').not.toMatch(/const\s+\w*PAGE\w*\s*=\s*\d+/);
  });

  /**
   * **初批必须覆盖视口**，否则分批是个陷阱：内容没撑出滚动条 ⇒ 永远收不到 scroll 事件 ⇒
   * 剩下的节点再也出不来，而用户看不出少了（没有滚动条就没有「还有更多」的暗示）。
   *
   * 真正的保证是那条「每次提交后按同一判据补批、收敛于撑出滚动条或取完」的 effect，
   * 而不是把每批数调大 —— 后者在 4K 满屏下照样不够。两条都钉。
   */
  it('有「补批到撑出滚动条为止」的 effect（不是靠把批量数调大蒙混）', () => {
    expect(NODES).toContain('const topUpBatch = useCallback(');
    // 装监听的那条 + 每次提交后补批的那条，两条都必须在。
    expect(NODES).toMatch(/scroller\.addEventListener\('scroll', topUpBatch/);
    expect(NODES).toContain('useIsomorphicLayoutEffect(topUpBatch)');
  });

  /**
   * **补批 effect 不得有依赖数组** —— 这一条是「放弃枚举、改观测结果」的落点。
   *
   * 曾经的做法是把「能改变网格高度」的向量枚举进依赖数组。两处实证否掉了它（详见 `NodesScreen`
   * 文件顶部「为什么放弃枚举」留档）：① `.side` 带 300ms 宽度过渡，`sidebarCollapsed` 那一帧量到的
   * 是折叠**前**的几何，枚举进去也收不住；② 一张列到 12 行的表仍漏了 `selectedServerId`
   * （`.nd-cur` chip 换卡 → 最高卡行数变 → `grid-auto-rows:1fr` 让**每一行**跟着变，15 行 × ~23px
   * = 345px，远超 240px 余量）。每加一颗角标就多一条要枚举的边，漏一条的后果是「用户永远点不到
   * 剩下的节点且看不出少了」。故不再钉具体依赖项，改钉「没有依赖项」。
   */
  it('补批 effect **没有依赖数组**（放弃枚举触发面，改观测结果）', () => {
    expect(NODES).toMatch(/useIsomorphicLayoutEffect\(topUpBatch\)\s*;/);
    expect(NODES, '又把补批退回按依赖枚举了 —— 见 NodesScreen 顶部「为什么放弃枚举」').not.toMatch(
      /useIsomorphicLayoutEffect\(topUpBatch\s*,/
    );
  });

  /**
   * 采样器② / ③ 的接线。**断言必须落在装监听那条 effect 的函数体切片内**，不能是几条互不约束的
   * 存在性 `toMatch`：那样把 `closest('.main-scroll')` 换成 `document.body`、或把
   * `ro.disconnect()` 从 cleanup 里挪到 `observe()` 后一行，全部照绿，而 RO 装错元素 ⇒ 侧栏折叠
   * 不再回报 ⇒ 第一轮那条 High 原样复发。
   */
  const scrollerEffect = (() => {
    const at = NODES.indexOf("scroller.addEventListener('scroll'");
    if (at < 0) return '';
    const start = NODES.lastIndexOf('useEffect(', at);
    const end = NODES.indexOf('\n  }, [', at);
    return start < 0 || end < 0 ? '' : NODES.slice(start, end);
  })();

  it('自检：装监听那条 effect 的函数体切到了（切不到则本节全部空跑）', () => {
    expect(scrollerEffect.length, '切片为空 —— effect 结构变了，下面几条已失去判据').toBeGreaterThan(
      200
    );
    expect(scrollerEffect).toContain('useEffect(');
  });

  it('采样器②：RO 观测的是 `closest(.main-scroll)` 拿到的那个元素，且在 cleanup 里 disconnect', () => {
    // 同一条 effect 内：scroller 来自 closest，RO 观测的就是它。
    expect(scrollerEffect).toMatch(
      /const scroller = gridRef\.current\?\.closest<HTMLElement>\('\.main-scroll'\)/
    );
    expect(scrollerEffect).toMatch(/new ResizeObserver\(topUpBatch\)/);
    expect(scrollerEffect, 'RO 装到别的元素上了 ⇒ 侧栏折叠不再回报').toMatch(
      /ro\.observe\(scroller\)/
    );
    // disconnect 必须在 cleanup 里 —— 位置比存在性重要。
    const ret = scrollerEffect.indexOf('return () => {');
    expect(ret, 'cleanup 不见了 ⇒ 切屏泄漏一个观测').toBeGreaterThan(-1);
    expect(
      scrollerEffect.slice(ret),
      'ro.disconnect() 不在 cleanup 里 ⇒ 装完就退订 / 或根本不退订'
    ).toContain('ro.disconnect()');
  });

  /**
   * `observe()` **不得带 `box` 参数**（默认 content-box）。`.main-scroll` 是 `overflow-y:auto`：
   * Win/Linux 经典滚动条下，内容从「不溢出」跨到「溢出」会让 content-box 宽缩约 15px —— 那一维
   * 恰恰要观测（网格变窄 ⇒ auto-fill 列数变 ⇒ 行数变）。改成 `border-box` 会整条丢掉它而全绿。
   */
  it('采样器②：`ro.observe` 不带 box 参数（border-box 会丢掉滚动条出现/消失这一维）', () => {
    expect(scrollerEffect, 'observe 带了 box 选项').not.toMatch(/ro\.observe\([^)]*box\s*:/);
    // 观测的必须是滚动容器，不是网格本身（后者才是真自激：追加内容直接撑高被观测元素）。
    expect(scrollerEffect, 'RO 观测到 `.node-grid` 上了 ⇒ 追加内容撑高它 ⇒ 通知循环').not.toMatch(
      /\.observe\(\s*gridRef\.current/
    );
  });

  /**
   * 采样器③，纯防御性保留（2026-08-17 更新，详见 NodesScreen.tsx 头注）。当年非它不可的实例是
   * 切视图档：`.nd-card{transition:.14s}` 曾是无 property 限定的简写 ⇒ `transition-property:all`，
   * 列表档把 `min-height` 141→0 一并改掉 ⇒ 那一次采样器①（每次 commit）量到的是**过渡前**的
   * 卡高，而采样器②只看容器盒子、内容变矮不改它。该处 transition 现已收窄到六个不参与盒模型的
   * 绘制层属性（border-color/box-shadow/background/border-radius/outline/outline-offset，
   * 完整清单见 screens.css:61），`view` 这一维今天不再触发这个洞，但③守的是机制本身（任何带
   * CSS 过渡的内容高度变化），不是这一个实例，故本条继续钉「监听确实挂着、且 cleanup 里摘掉」。
   */
  it('采样器③：委派 `transitionend` 在同一条 effect 里挂上、且在 cleanup 里摘掉', () => {
    expect(scrollerEffect, '带 CSS 过渡的内容高度变化没有采样器').toMatch(
      /scroller\.addEventListener\('transitionend', topUpBatch\)/
    );
    const ret = scrollerEffect.indexOf('return () => {');
    expect(scrollerEffect.slice(ret), 'transitionend 监听没摘 ⇒ 切屏泄漏').toMatch(
      /scroller\.removeEventListener\('transitionend', topUpBatch\)/
    );
  });

  it('采样器②取代了 `window resize`（旧监听是它的子集，留着就是第二条要同步的路径）', () => {
    expect(NODES).not.toMatch(/window\.addEventListener\('resize'/);
  });

  it('每批数至少覆盖窗口最小尺寸下的一屏（980×740、200px 最小列宽、141px 卡高）', () => {
    // 4 列 × 5 行 = 20；取整数余量后仍应远小于每批数。这条只挡「有人把每批数调到个位数」，
    // 视口覆盖的真正保证是上一条的补批 effect。
    const worstCaseFirstScreen = 4 * 5;
    expect(SCROLL_BATCH_PAGE).toBeGreaterThanOrEqual(worstCaseFirstScreen);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 门 4 · 真渲染首帧：分批确实生效，且空态/全集判断没被切片带偏
 * ════════════════════════════════════════════════════════════════════════════ */

/** 造 N 个自建 vless 节点（无 subscriptionId ⇒ 落「自建」组）。 */
function makeServers(n: number): ServerConfig[] {
  return Array.from({ length: n }, (_, i) => ({
    id: `n${i + 1}`,
    name: `批量节点 ${String(i + 1).padStart(3, '0')}`,
    protocol: 'vless' as const,
    address: `n${i + 1}.example.net`,
    port: 443,
    uuid: `demo-uuid-${i + 1}`,
    encryption: 'none',
  }));
}

describe('门 4 · 首帧真渲染（react-dom/server，node 环境，无 jsdom）', () => {
  const TOTAL = SCROLL_BATCH_PAGE * 2 + 7;

  it(`${TOTAL} 个节点的首帧只画首批 ${SCROLL_BATCH_PAGE} 张卡`, async () => {
    const servers = makeServers(TOTAL);
    const config: UserConfig = { ...DEMO_CONFIG, servers, subscriptions: [], selectedServerId: 'n1' };
    const seed = { config, servers, selectedServerId: 'n1', rules: config.customRules };
    const { useAppStore } = await import('@/store/app-store');
    useAppStore.setState(seed);
    // zustand v4 在服务端渲染下读初始态快照，只 setState 会对着空 store 渲染（同 initial-tab-first-frame）。
    Object.assign(useAppStore.getInitialState(), seed);
    const NodesScreen = (await import('./NodesScreen')).default;

    const html = renderToStaticMarkup(<NodesScreen />);
    // 自曝：真的渲出了节点网格（渲染失败/空态时下面的计数恒 0，会把断言骗成"分批很激进"）。
    expect(html).toContain('node-grid');
    expect(html).not.toContain('nodes.empty');
    expect(occurrences(html, 'class="nd-card')).toBe(SCROLL_BATCH_PAGE);
    // 正向对照：总数确实超过一批，否则上一条无信息量。
    expect(TOTAL).toBeGreaterThan(SCROLL_BATCH_PAGE);
    // 首批是**前** SCROLL_BATCH_PAGE 个（切的是尾，不是随机子集）。
    expect(html).toContain('批量节点 001');
    expect(html).toContain(`批量节点 ${String(SCROLL_BATCH_PAGE).padStart(3, '0')}`);
    expect(html).not.toContain(`批量节点 ${String(SCROLL_BATCH_PAGE + 1).padStart(3, '0')}`);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 门 5 · NodeMenu 折叠分组不挂载（DOM 节点 + fiber 都不建，不是靠 hidden 藏）
 * ════════════════════════════════════════════════════════════════════════════ */

describe('门 5 · 首页出口选单：折叠分组整棵子树不挂载', () => {
  it('折叠态下**一个** `.nm-item` 都不渲染（原状是全渲染 + hidden 藏）', async () => {
    const { NodeMenu } = await import('../home/NodeMenu');
    const servers = makeServers(12);
    const html = renderToStaticMarkup(
      <NodeMenu
        open
        servers={servers}
        subscriptions={[]}
        selectedServerId={null}
        latencies={{}}
        onPick={() => {}}
        onPickDirect={() => {}}
        onPickBlock={() => {}}
        blockDisabledReason={null}
        onTestAll={() => {}}
        onManage={() => {}}
      />
    );
    // 自曝：分组头确实渲出来了（否则整个组件没渲染，下面的负向断言恒真）。
    expect(html).toContain('ns-grp');
    // 变异对照：改回 `items.map(...)` + `hidden={!isOpen}` ⇒ 12 个 nm-item 出现 ⇒ 本条转红。
    expect(occurrences(html, 'nm-item')).toBe(0);
    expect(html).not.toContain('批量节点 001');
  });

  it('检索真值不受影响：搜索态强制展开那条腿仍在，且「全部测速」读数据不读 DOM', () => {
    // `isOpen` 同时是 map 的门与搜索态的出口 —— 两者必须是同一个变量，否则搜出来的节点会被折叠吃掉。
    expect(MENU).toContain('const isOpen = searching || openGroups.has(g.id);');
    expect(MENU).toContain('{isOpen && items.map((s) => {');
    expect(MENU).toContain('onTestAll(filteredGroups.flatMap((g) => g.servers.map((s) => s.id)))');
  });
});
