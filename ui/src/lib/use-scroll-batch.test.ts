/**
 * 分批渲染的门 —— 两件事：**行为**（分批真的会推进、复位在渲染期、到顶不自激）与那条**真机实证**
 * （不许用 `loading="lazy"` / IntersectionObserver）。
 *
 * # 为什么必须有行为断言（这一节别删）
 *
 * 源码门只能回答「接线在不在」。而本 hook 的两种状态 —— 「会推进」与「永远只有首批」——
 * 在本仓可观测的那一层**逐字一致**：`nodes-render-budget.test.tsx` 门 4 用 `renderToStaticMarkup`
 * 真渲染，SSR 下 effect 不跑，它钉的「首帧只有 60 张」在两种状态下都成立。于是把推进条件写成恒假、
 * 或把复位改成每次渲染都跑，`pnpm test` 全绿而真机永远只显示前 60 个节点。
 * 故判据提成纯函数（`shouldAdvance` / `advanceBatch`）直测**正反两向**，再用一个极简宿主驱动
 * **真实的 hook 源码**（不是复刻一份判据）验计数怎么走。
 *
 * # 极简宿主为什么长这样
 *
 * 本仓 vitest 是 node 环境、无 jsdom，且不为一道门引新依赖（react-test-renderer / testing-library）。
 * `useScrollBatch` 只用 `useState`，故把 react 的 `useState` 换成「数组游标 + 重跑到 state 稳定」
 * 的 30 行实现即可 —— 「重跑到稳定」正是 React 对渲染期 `setState` 的语义，复位那一段因此被真的走到。
 *
 * # 为什么那条 lazy 判据只能靠源码门
 *
 * 真机（macOS/WKWebView）症状是：请求根本没到 scheme handler，却触发了 img 的 `onerror`
 * —— 屏幕上是一片白方块，本机（Linux/Chromium）与任何单测里都**复现不了**。
 * 换回 `loading="lazy"` 类型对、构建过、全部测试绿，只有真机会坏。
 * 判据只写在注释里挡不住下一个人「顺手加个 lazy 优化一下」，故落成门。
 *
 * # 射程（如实记账）
 *
 * 扫的是**登记在 CONSUMERS 里的消费方文件**里有没有出现那两个词。抓不到：
 *  · 别的文件里用 IntersectionObserver（本仓其它地方若真有正当用途，不该被这道门连坐 ——
 *    坑的成因是「top-layer `<dialog>` + 小高度滚动容器」这个组合，不是这两个 API 本身）；
 *  · 动态构造属性名（`el.setAttribute('loading', 'lazy')`）；
 *  · **消费方渲染的子组件里的 lazy** —— 节点网格里真正带 `<img>` 的是 `NodeCard`（经 `nd-flag.tsx`
 *    的国旗图），`NodesScreen` 本身一个 `<img>` 都没有。就「真机白块」这个被守的缺陷而言，
 *    把 `NodesScreen` 登记进来射程为零；它进 CONSUMERS 是为了另外两条（复用唯一实现、不自持常量）。
 */
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/* ────────────────────────────────────────────────────────────────────────────
 * 极简 hook 宿主（见头注）。`vi.hoisted` 是因为 `vi.mock` 的工厂被提升到 import 之前，
 * 不能闭包引用普通的模块级变量。
 * ──────────────────────────────────────────────────────────────────────────── */

const host = vi.hoisted(() => ({ slots: [] as unknown[], cursor: 0, dirty: false }));

vi.mock('react', () => ({
  /**
   * 宿主刻意**不跑** effect：本组断言问的是「提交出去的那一帧长什么样」，而 passive effect 按定义
   * 跑在提交之后。所以复位一旦退回 `useEffect(() => setCount(PAGE), [resetKey])`，这里看到的就是
   * 真机上那一帧 —— 按上一档的大计数画的 —— 断言当场转红。留这个空壳而不是让 hook 报「没有该导出」，
   * 是为了让红的**原因**落在语义上，不落在 mock 缺项上。
   */
  useEffect: () => {},
  useState: (init: unknown) => {
    const i = host.cursor++;
    if (i >= host.slots.length) host.slots[i] = init;
    const set = (v: unknown) => {
      const prev = host.slots[i];
      const next = typeof v === 'function' ? (v as (p: unknown) => unknown)(prev) : v;
      // 与 React 一致：同值不产生新渲染（本 hook「到顶不自激」正是靠这条）。
      if (!Object.is(next, prev)) {
        host.slots[i] = next;
        host.dirty = true;
      }
    };
    return [host.slots[i], set];
  },
}));

import {
  useScrollBatch,
  shouldAdvance,
  advanceBatch,
  SCROLL_BATCH_PAGE,
  SCROLL_BATCH_AHEAD_PX,
} from './use-scroll-batch';

/** 新挂载一个 hook 实例（清空 state 槽）。 */
function mount(): void {
  host.slots = [];
}

/** 一次「渲染 + 提交」：重跑组件直到 state 不再变 —— React 对渲染期 `setState` 就是这个语义。 */
function commit<T>(render: () => T, maxPasses = 20): T {
  for (let pass = 0; pass < maxPasses; pass++) {
    host.cursor = 0;
    host.dirty = false;
    const out = render();
    if (!host.dirty) return out;
  }
  throw new Error('渲染期 setState 没有收敛（>20 趟）—— 复位被写成无条件调用了？');
}

/** 距底 0px：该追加。 */
const AT_BOTTOM = { currentTarget: { scrollHeight: 800, scrollTop: 0, clientHeight: 800 } };
/** 距底 7200px：不该追加。 */
const FAR_FROM_BOTTOM = { currentTarget: { scrollHeight: 8000, scrollTop: 0, clientHeight: 800 } };

/* ════════════════════════════════════════════════════════════════════════════
 * 行为① 推进判据（纯函数，正反两向 —— 少任一向，恒真/恒假的实现都能过）
 * ════════════════════════════════════════════════════════════════════════════ */

describe('推进判据 `shouldAdvance`', () => {
  /** 距底 d 像素的滚动容器。 */
  const at = (d: number) => ({ scrollHeight: 1000 + d, scrollTop: 0, clientHeight: 1000 });

  it('距底 ≤ 预取余量 → 真', () => {
    expect(shouldAdvance(at(0))).toBe(true);
    expect(shouldAdvance(at(1))).toBe(true);
    expect(shouldAdvance(at(SCROLL_BATCH_AHEAD_PX))).toBe(true);
  });

  it('距底 > 预取余量 → 假（恒真实现会在这里转红）', () => {
    expect(shouldAdvance(at(SCROLL_BATCH_AHEAD_PX + 1))).toBe(false);
    expect(shouldAdvance(at(10_000))).toBe(false);
  });

  /**
   * **量不到就不判**。全零输入下 `0 - 0 - 0 <= 240` 恒真 ⇒「不该推进」这条分支永不成立 ⇒
   * `advanceBatch` 的 bail-out 只剩 `c >= total`，而节点屏的补批循环跑在 layout 档、同步、无上限：
   * total=3000 要 50 轮，正好撞 React 的 `NESTED_UPDATE_LIMIT = 50` ⇒ `Maximum update depth
   * exceeded`（白屏），不是「就地 bail-out」。真实入口是 RO：被观测元素停止渲染时投递 0×0。
   */
  it('容器高度为 0 时不推进（否则补批循环撞 React 的嵌套更新上限）', () => {
    expect(shouldAdvance({ scrollHeight: 0, scrollTop: 0, clientHeight: 0 })).toBe(false);
    expect(shouldAdvance({ scrollHeight: 0, scrollTop: 0, clientHeight: -1 })).toBe(false);
    // 正向对照：同样「距底 0」但容器有真实高度时**必须**推进，否则上面两条只是把判据写死成假。
    expect(shouldAdvance({ scrollHeight: 800, scrollTop: 0, clientHeight: 800 })).toBe(true);
    // 到了 advanceBatch 这一层同样不动（这才是循环真正读的那个出口）。
    expect(advanceBatch(60, 3000, { scrollHeight: 0, scrollTop: 0, clientHeight: 0 })).toBe(60);
  });

  it('`scrollTop` 计入距底：内容再长，滚到底也该追加', () => {
    expect(shouldAdvance({ scrollHeight: 10_000, scrollTop: 8_900, clientHeight: 1_000 })).toBe(
      true
    );
    // 正向对照：同一份内容没滚时不该追加，否则上一条只是恒真。
    expect(shouldAdvance({ scrollHeight: 10_000, scrollTop: 0, clientHeight: 1_000 })).toBe(false);
  });

  it('`advanceBatch`：不该推进 / 已取完，都返回**原值**（同值 ⇒ React 就地 bail-out ⇒ 不自激）', () => {
    const m = AT_BOTTOM.currentTarget;
    expect(advanceBatch(60, 1000, FAR_FROM_BOTTOM.currentTarget)).toBe(60);
    expect(advanceBatch(60, 1000, m)).toBe(120);
    expect(advanceBatch(240, 187, m)).toBe(240);
    expect(advanceBatch(187, 187, m)).toBe(187);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 行为② 真实 hook 的计数怎么走（极简宿主驱动，跑的是 use-scroll-batch.ts 的源码）
 * ════════════════════════════════════════════════════════════════════════════ */

describe('`useScrollBatch` 计数推进', () => {
  it('滚到底 → 60 → 120 → 180 → 到 total 后**不再增长**', () => {
    mount();
    const total = SCROLL_BATCH_PAGE * 3 + 7; // 187：刻意不是整数批，验「越过 total 即停」
    const seen: number[] = [];
    let batch = commit(() => useScrollBatch(total, 'k'));
    seen.push(batch.count);
    for (let i = 0; i < 5; i++) {
      batch.onScroll(AT_BOTTOM);
      batch = commit(() => useScrollBatch(total, 'k'));
      seen.push(batch.count);
    }
    expect(seen).toEqual([60, 120, 180, 240, 240, 240]);
    // 正向对照：total 确实超过一批，否则上面这串没有信息量。
    expect(total).toBeGreaterThan(SCROLL_BATCH_PAGE);
  });

  it('没滚到底就不推进（少了这一向，「每次渲染都追加」照样能过上一条）', () => {
    mount();
    let batch = commit(() => useScrollBatch(1000, 'k'));
    batch.onScroll(FAR_FROM_BOTTOM);
    batch = commit(() => useScrollBatch(1000, 'k'));
    expect(batch.count).toBe(SCROLL_BATCH_PAGE);
  });

  it('`resetKey` 变的**那一次提交**就已经是首批（复位在渲染期，不留按旧计数的那一帧）', () => {
    mount();
    let batch = commit(() => useScrollBatch(1000, 'a'));
    for (let i = 0; i < 3; i++) {
      batch.onScroll(AT_BOTTOM);
      batch = commit(() => useScrollBatch(1000, 'a'));
    }
    expect(batch.count).toBe(SCROLL_BATCH_PAGE * 4);
    // 复位若退回 `useEffect`，这里会先拿到 240 —— 真机上就是「先按上一档的大计数画完整整一帧，
    // 再回落到 60」，正好是分批要消掉的那一帧。
    batch = commit(() => useScrollBatch(1000, 'b'));
    expect(batch.count).toBe(SCROLL_BATCH_PAGE);
  });

  /**
   * **一次采样不够，必须能被重复回调**（这条是「补批为什么改用 ResizeObserver 观测、不靠
   * 依赖数组枚举」的行为面判据）。侧栏 `.side` 带 `transition:width .3s ease-out`：折叠那一次
   * commit 后立刻量，过渡 progress=0 ⇒ 量到的是折叠**前**的几何 ⇒ 判「仍溢出」不补批；
   * 300ms 后列数 4→5、行数 15→12、内容矮 423px ⇒ 不再溢出。若那时没有第二次回调就永久卡死。
   *
   * 射程如实记账：本条钉的是**判据**（同一状态下两次不同几何的采样各自该怎么走），
   * 「回调真的挂在 `.main-scroll` 上」是 DOM 接线，由 `nodes-render-budget.test.tsx` 的源码门钉。
   */
  it('侧栏折叠时序：t=0 量到旧几何不推进，过渡结束后再量一次才推进', () => {
    mount();
    const total = 300;
    let batch = commit(() => useScrollBatch(total, 'k'));
    // 收敛态（窄窗 4 列、60 张卡 15 行 = 15×141 + 14×12(gap) = 2283px，视口 1800 ⇒ 距底 483 > 240）。
    batch.onScroll({ currentTarget: { scrollHeight: 2283, scrollTop: 0, clientHeight: 1800 } });
    batch = commit(() => useScrollBatch(total, 'k'));
    expect(batch.count, 't=0 量到的是折叠前的几何 ⇒ 本就不该补批').toBe(SCROLL_BATCH_PAGE);
    // 过渡结束：5 列 12 行 = 12×141 + 11×12 = 1824px < 1800+240 ⇒ 该补批，此刻必须还有人来敲一次。
    batch.onScroll({ currentTarget: { scrollHeight: 1824, scrollTop: 0, clientHeight: 1800 } });
    batch = commit(() => useScrollBatch(total, 'k'));
    expect(batch.count, '过渡结束后的那次回调没能补批 ⇒ 剩下 240 个节点永久点不到').toBe(
      SCROLL_BATCH_PAGE * 2
    );
  });

  /**
   * 同一形状的第二个场景：**带 CSS 过渡的内容高度**。本条钉的是这类场景下的判据本身（有过渡的
   * 高度变化必须靠 `transitionend` 补第二次采样），场景本身是虚构的、与具体是哪个 CSS 选择器/
   * 属性在过渡无关——测的是机制不是实例，标题与断言措辞都不点名具体场景。
   *
   * 2026-08-17 更新：曾经确实有一个真实实例（`.nd-card` 切视图档，列表档把 `min-height` 141→0
   * 一并改掉），但该处 transition 已收窄到六个不参与盒模型的绘制层属性（screens.css:61），今天
   * `view` 这一维不再触发这个场景。本条不因此删——测的是「有过渡的高度变化」这整条机制在
   * `useScrollBatch` 侧的判据是否正确，不依赖仓内此刻是否存在满足该形状的真实 CSS，机制本身
   * 仍由 NodesScreen.tsx 头注「采样器③」段的理由保留。
   * 「监听真的挂在 `.main-scroll` 上、且在 cleanup 里摘掉」由 `nodes-render-budget.test.tsx` 钉。
   */
  it('有过渡的内容高度：commit 那次量到中途值不推进，transitionend 补一次才推进', () => {
    mount();
    const total = 300;
    let batch = commit(() => useScrollBatch(total, 'k'));
    // commit 那一刻：过渡还在中途，量到的高度仍远超视口 ⇒ 判「不推进」。
    batch.onScroll({ currentTarget: { scrollHeight: 3000, scrollTop: 0, clientHeight: 1200 } });
    batch = commit(() => useScrollBatch(total, 'k'));
    expect(batch.count, 'commit 那次量到的是过渡中途值').toBe(SCROLL_BATCH_PAGE);
    // 过渡结束：高度跌到视口以内 ⇒ 没有第三个采样器就永久卡死。
    batch.onScroll({ currentTarget: { scrollHeight: 900, scrollTop: 0, clientHeight: 1200 } });
    batch = commit(() => useScrollBatch(total, 'k'));
    expect(batch.count, 'transitionend 那次没能补批 ⇒ 过渡结束后再也点不到剩下的节点').toBe(
      SCROLL_BATCH_PAGE * 2
    );
  });

  /**
   * `resetKey` 必须是原始值 —— 类型已收窄到 [`ScrollBatchResetKey`]，本条钉的是**收窄的理由**：
   * 复位改到渲染期后判等走 `Object.is`，传对象字面量 ⇒ 每次渲染都是新引用 ⇒ 每次渲染都复位并
   * `setState` ⇒ 渲染期自激。真机上 React 抛 `Maximum update depth exceeded`（白屏）；
   * 本宿主的表现是「重跑 20 趟仍不收敛」。少了这条，将来有人加个 `as any` 绕过类型就没人拦。
   */
  it('`resetKey` 传对象引用会渲染期自激（故类型收窄成原始值）', () => {
    mount();
    // 组件体里每次求值都造一个新对象（`useScrollBatch(n, { tab, search })` 就是这形状）。
    const render = () => useScrollBatch(100, {} as unknown as string);
    // 首次提交把槽初始化成那一次的引用，收敛；**下一次**提交起每趟重跑都是新引用 ⇒ 永不收敛。
    expect(commit(render).count).toBe(SCROLL_BATCH_PAGE);
    expect(() => commit(render)).toThrowError(/没有收敛/);
  });

  it('`renderAll` 一次取到底（消费方找不到滚动祖先时的 fail-open），且之后不再增长', () => {
    mount();
    const total = SCROLL_BATCH_PAGE * 3 + 7;
    let batch = commit(() => useScrollBatch(total, 'k'));
    batch.renderAll();
    batch = commit(() => useScrollBatch(total, 'k'));
    expect(batch.count).toBe(total);
    batch.renderAll();
    batch = commit(() => useScrollBatch(total, 'k'));
    expect(batch.count).toBe(total);
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 源码门：唯一实现 + 禁 lazy/IntersectionObserver（射程见头注）
 * ════════════════════════════════════════════════════════════════════════════ */

/** 登记的消费方（新增一个就加一行 —— 手动登记是为了让「谁在分批」这件事留在灯下）。 */
const CONSUMERS = [
  '../components/dialogs/AppAddDialog.tsx',
  '../components/dialogs/RuleDialog.tsx',
  // 第三个消费方：节点网格。与前两个不同，它的滚动容器是 `AppShell` 的 `.main-scroll`（祖先元素），
  // 故那边走原生 addEventListener 而不是 JSX 的 `onScroll=`，但用的是同一个 hook、同一份常量。
  '../components/screens/nodes/NodesScreen.tsx',
] as const;

const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');

const sources = CONSUMERS.map((rel) => ({
  rel,
  src: code(readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8')),
}));

describe('滚动分批：唯一实现 + 禁 lazy/IntersectionObserver', () => {
  it('自检：登记的消费方都读到了，且都真的在用这个 hook（否则下面是空跑）', () => {
    for (const { rel, src } of sources) {
      expect(src.length, `${rel} 读空了`).toBeGreaterThan(2000);
      expect(src, `${rel} 不再消费 useScrollBatch —— 是不是又抄了一份内联实现？`).toContain(
        'useScrollBatch('
      );
    }
  });

  it('消费方不得出现 `loading="lazy"` / IntersectionObserver（真机白块的元凶）', () => {
    const offenders: string[] = [];
    for (const { rel, src } of sources) {
      if (/loading\s*=\s*["'{]?\s*["']?lazy/.test(src)) offenders.push(`${rel}: loading="lazy"`);
      if (/IntersectionObserver/.test(src)) offenders.push(`${rel}: IntersectionObserver`);
    }
    expect(
      offenders,
      '真机（macOS/WKWebView）实测：在 top-layer <dialog> + 小高度滚动容器里，请求到不了 ' +
        'scheme handler 却触发 onerror ⇒ 一片白方块。IntersectionObserver 与 lazy 同族，' +
        '不拿它做替代。分批用纯 scroll 事件（lib/use-scroll-batch.ts）。'
    ).toEqual([]);
  });

  it('消费方不得再自持分批常量（第二份常量 = 两处滚动手感会漂）', () => {
    for (const { rel, src } of sources) {
      expect(src, `${rel} 又自持了一份每批数`).not.toMatch(/GALLERY_PAGE|GALLERY_LOAD_AHEAD_PX/);
    }
  });

  it('常量在合理量级（写成 0 会让分批永不推进 = 列表只剩空白）', () => {
    expect(SCROLL_BATCH_PAGE).toBeGreaterThan(10);
    expect(SCROLL_BATCH_AHEAD_PX).toBeGreaterThan(0);
  });
});
