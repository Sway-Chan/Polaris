/**
 * 测速接线不变量守卫 —— 钉死真机两条指控的**接线形态**，防回归。
 *
 * 用户 2026-07-21 真机原话：「测速逻辑也没对齐，主窗口只测速当前节点，同时测试结果应该是所有节点
 * 都应该复用消费」。两条根因都**不在算法里，在接线里**：
 *  1. 后端批量编排一直是好的，是 `HomeScreen` 硬编码 `speedTest([currentServer.id])`；
 *  2. 结果存在四份组件私有 `useState`，订阅寿命 = 组件挂载寿命，切屏即丢。
 *
 * 这类缺陷**逻辑单测抓不到**（纯函数各自都对，错的是谁调谁、状态放哪），而组件级测试要在本仓
 * 拉起 jsdom + 十几个 mock，成本远高于收益。故沿用本仓既有的**源码不变量守卫**模式
 * （`styles/style-invariants.test.ts`、`i18n/locale-parity.test.ts`、`lib/proxy-start-claim.test.ts`）：
 * 直接对源文件断言接线形态。
 *
 * 守的是**形态**不是措辞：断言的都是「调了哪个函数 / 状态存在哪」这类结构事实，
 * 改注释、改变量名、改文案都不会误伤。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const SRC = resolve(__dirname, '..');
const read = (rel: string): string => readFileSync(resolve(SRC, rel), 'utf8');

const HOME = 'components/screens/home/HomeScreen.tsx';
const NODES = 'components/screens/nodes/NodesScreen.tsx';
/**
 * 节点页延迟的**真实消费点**。2026-08-16 那批把延迟消费从 `NodesScreen` 搬进单卡（每张卡按自身
 * id 细粒度订阅），`NodesScreen` 只剩 `sortKey === 'lat'` 那条**条件**订阅 —— 下面两组 `it.each`
 * 若不跟着搬，2026-07-21 真机那条「结果存组件私有 state、切屏即丢」的缺陷可以在新位置原样复活，
 * 而两道门都不响：门守的是文件，而真值落地的文件换了。
 */
const NODE_CARD = 'components/screens/nodes/NodeCard.tsx';
const STATUSBAR = 'components/layout/StatusBar.tsx';
const TRAY = 'tray/TrayMenu.tsx';
const APP = 'App.tsx';

/** 五个消费方 + App（订阅所在地）。 */
const CONSUMERS = [HOME, NODES, NODE_CARD, STATUSBAR, TRAY];

/**
 * T1（**2026-07-31 语义反转**，陈先生裁定）：主页有**两颗**测速入口，射程刻意不同 ——
 * 「网络检测」= 当前出口单节点，出口选单的「全部测速」= 全量。
 *
 * 本条此前钉的是相反的不变量（「主页测速必须是全量集合，不是当前节点」，含一条
 * `not.toMatch(speedTest([currentServer.id]))` 的负向断言）。那条是 1:1 移植 上游
 * `connection-control-card.tsx:156` 的产物，而 上游 那颗按钮只做延迟；Polaris 把它与解锁重检、
 * 出口 IP 重探合并成一颗「网络检测」，另两条腿本就只针对当前出口 —— 延迟腿仍全量就是同一次点击
 * 里两种射程，且与选单里的「全部测速」逐字同集合（那颗按钮的合并理由恰恰是「不要重复入口」）。
 *
 * 射程的完整判据（三条边界如实回报 / 分支顺序）在专文
 * `components/screens/home/home-speedtest-scope.test.ts`；此处只钉**接线拓扑**这一面。
 */
describe('T1：主页两颗测速入口射程不同（网络检测=当前出口 / 全部测速=全量）', () => {
  it('网络检测腿只请求当前出口（**不得**退回 speedTestableIds 全量）', () => {
    const src = read(HOME);
    expect(src).toContain('api.server.speedTest([currentServer.id])');
  });

  it('全量入口仍在（菜单「全部测速」经 speedTestableIds 过滤）', () => {
    expect(read(HOME)).toContain('speedTestableIds(servers');
  });

  it('两条腿的可测性判定都 path-aware（带 mainCorePool 能力位，否则 TS-exit 会被误纳/误排）', () => {
    const src = read(HOME);
    expect(src).toMatch(/speedTestableIds\([^)]*mainCorePool/s);
    expect(src).toMatch(/speedTestBlockReason\([^)]*mainCorePool/s);
  });
});

describe('T2：latencyMap 是全局单一真值，不是组件私有 state', () => {
  it.each(CONSUMERS)('%s 读全局 store', (f) => {
    expect(read(f)).toContain('useLatencyStore');
  });

  it.each(CONSUMERS)('%s **不得**把延迟存回组件私有 useState', (f) => {
    const src = read(f);
    // 原状形态：`const [latencies, setLatencies] = useState<Record<string, number...>>({})`
    // 及 StatusBar 的 `const [latencyMs, setLatencyMs] = useState<...>`。
    expect(src).not.toMatch(/useState<Record<string,\s*number[^>]*>>\s*\(/);
    expect(src).not.toMatch(/\[\s*latencies\s*,\s*setLatencies\s*\]/);
    expect(src).not.toMatch(/\[\s*latencyMs\s*,\s*setLatencyMs\s*\]/);
  });
});

describe('T2：订阅挂窗口级持久位置，不得退回业务组件', () => {
  it('主窗订阅在 App.tsx（全窗口生命周期只建一次）', () => {
    expect(read(APP)).toContain('subscribeLatencyEvents()');
  });

  it('托盘窗订阅在 TrayMenu 根（独立 JS 堆，本窗自己的持久位置）', () => {
    expect(read(TRAY)).toContain('subscribeLatencyEvents()');
  });

  it.each([HOME, NODES, NODE_CARD, STATUSBAR])(
    '%s **不得**自建 onSpeedTestResult 订阅（挂组件里 = 卸载即断 = 切屏丢结果）',
    (f) => {
      // 匹配**调用**形态而非任意提及：注释里写「勿改回 onSpeedTestResult 私有订阅」不该误伤。
      expect(read(f)).not.toMatch(/\.onSpeedTestResult\s*\(/);
    }
  );
});
