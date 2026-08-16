/**
 * 拓扑渲染预算门 —— 守住「首页拓扑每帧只做必要的那点活」。
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
 * # 本机实测口径（Playwright Chromium 149 + WebKit 26.5，**双引擎数字完全一致**，Top-15 满载帧、topo 容器 1052px）
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
    // 正向对照：props 一个都没解析出来时上面的断言恒真，故先钉住已知的两个。
    expect(props.map(([n]) => n).sort()).toEqual(['aggregate', 'disconnected']);
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

describe('准确性门 · 常态 Top-N 只负责绘制，不得充当搜索真值', () => {
  it('非空查询等待完整表变化监听就绪，再走后端过滤投影', () => {
    const ready = TOPO_SRC.indexOf('onConnectionsTopologyChangedReady');
    const search = TOPO_SRC.indexOf('searchTopology(normalizedSearch)');
    expect(ready).toBeGreaterThan(0);
    expect(search).toBeGreaterThan(0);
    expect(TOPO_SRC).toContain('searchProjection.aggregate');
    expect(TOPO_SRC).toContain("t('home.connectionFlow')");
  });

  it('完整表零结果回包前不得用 Top-N 猜测“无命中”', () => {
    expect(TOPO_SRC).toContain('searching && searchResolved && displayAggregate?.total === 0');
  });
});
