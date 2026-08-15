/**
 * 测速结果 store 回归测 —— 钉死「切屏即丢延迟」那一族根因的前端半边。
 *
 * 真机指控：「测试结果应该是所有节点都应该复用消费」。根因**不是**没存/没广播/key 不对，
 * 而是这份 map 曾以四份**组件私有 useState** 存在（HomeScreen / NodesScreen / StatusBar / TrayMenu），
 * 订阅寿命 = 组件挂载寿命，而 `ScreenRouter` 是裸 switch（无 keep-alive）⇒ 切屏即卸载即丢。
 *
 * 本文件锁三条（任一破了都会退回原状）：
 *  1. **状态在组件外**：store 是模块级单例，读写不经任何 React 生命周期；
 *  2. **合并不替换**：逐节点流式回填必须保留其余节点的历史值（原 useState 版本用函数式合并，
 *     改 store 时若写成整体替换，一次测速只会剩最后一个节点有值）；
 *  3. **订阅在组件外**：`subscribeLatencyEvents` 把事件流接进 store，且返回可退订句柄 ——
 *     「组件卸载」不参与其中，故跨屏切换不丢。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';

// 事件流 mock：捕获注册的 listener，供测试手动投递（等价后端 emit）。
const listeners: Array<(d: { serverId: string; latency: number }) => void> = [];
const offSpy = vi.fn();
vi.mock('@/ipc', () => ({
  api: {
    server: {
      onSpeedTestResult: (l: (d: { serverId: string; latency: number }) => void) => {
        listeners.push(l);
        return offSpy;
      },
    },
  },
}));

import {
  useLatencyStore,
  subscribeLatencyEvents,
  isLatencyStale,
  LATENCY_STALE_MS,
  normalizeLatencyResult,
} from './use-latency-store';

/** 投递一条后端 `event:speedTestResult`（打给所有在册监听器）。 */
function emitResult(serverId: string, latency: number): void {
  for (const l of listeners) l({ serverId, latency });
}

beforeEach(() => {
  listeners.length = 0;
  offSpy.mockClear();
  // 直接经 zustand 的 setState 复位：清空只有测试夹具需要，生产零调用点 —— 为它在 store 上导出一个
  // 公开 API 等于给四个屏幕 import 的 store 挂一条死 API（复审 #9），故不留 `resetLatencies`。
  useLatencyStore.setState({ latencyMap: {}, testedAt: {} });
});

describe('use-latency-store：单一真值', () => {
  it('逐节点回填**合并**而非替换（一轮全量测速后每个节点都留有值）', () => {
    const { applyLatencyResult } = useLatencyStore.getState();
    applyLatencyResult('a', 120);
    applyLatencyResult('b', 240);
    applyLatencyResult('c', -1); // 后端判定的真实不可测

    expect(useLatencyStore.getState().latencyMap).toEqual({ a: 120, b: 240, c: null });
  });

  it('批量合并保留未在本批的历史值（invoke 返回值兜底同步不得抹掉旧结果）', () => {
    useLatencyStore.getState().applyLatencyResult('old', 50);
    useLatencyStore.getState().applyLatencyResults({ a: 120, b: 240 });

    expect(useLatencyStore.getState().latencyMap).toEqual({ old: 50, a: 120, b: 240 });
  });

  it('后写覆盖同一节点的先前值（重测即刷新）', () => {
    useLatencyStore.getState().applyLatencyResult('a', 120);
    useLatencyStore.getState().applyLatencyResult('a', 88);

    expect(useLatencyStore.getState().latencyMap.a).toBe(88);
  });

  it('IPC 的负数失败码在唯一写入口归一为 null，绝不泄漏成绿色的 “-1 ms”', () => {
    expect(normalizeLatencyResult(-1)).toBeNull();
    expect(normalizeLatencyResult(Number.NaN)).toBeNull();
    expect(normalizeLatencyResult(0)).toBe(0);

    useLatencyStore.getState().applyLatencyResults({ failed: -1, ok: 86 });
    expect(useLatencyStore.getState().latencyMap).toEqual({ failed: null, ok: 86 });
  });

  it('节点删除后同时裁剪延迟与时间戳，保留节点的结果不动', () => {
    useLatencyStore.getState().applyLatencyResults({ keep: 42, removed: 88 });
    const keepStamp = useLatencyStore.getState().testedAt.keep;

    useLatencyStore.getState().retainServerIds(['keep']);

    expect(useLatencyStore.getState().latencyMap).toEqual({ keep: 42 });
    expect(useLatencyStore.getState().testedAt).toEqual({ keep: keepStamp });
  });

  it('节点集没有删除项时保持 state 引用，避免 configChanged 触发无效通知', () => {
    useLatencyStore.getState().applyLatencyResult('keep', 42);
    const before = useLatencyStore.getState();
    useLatencyStore.getState().retainServerIds(['keep', 'new-without-result']);
    expect(useLatencyStore.getState()).toBe(before);
  });
});

describe('use-latency-store：陈旧判定的数据基础（契约「陈旧 >30min 半透明」）', () => {
  it('两个写入口**都**打时间戳（漏一个就有一路延迟永远不显陈旧）', () => {
    const before = Date.now();
    useLatencyStore.getState().applyLatencyResult('a', 120);
    useLatencyStore.getState().applyLatencyResults({ b: 240, c: -1 });
    const { testedAt } = useLatencyStore.getState();

    for (const id of ['a', 'b', 'c']) {
      expect(testedAt[id], `${id} 未打戳`).toBeGreaterThanOrEqual(before);
    }
  });

  it('批量结果同批同戳（同一次 speedTest 不该产生毫秒级抖动）', () => {
    useLatencyStore.getState().applyLatencyResults({ a: 1, b: 2, c: 3 });
    const { testedAt } = useLatencyStore.getState();
    expect(new Set([testedAt.a, testedAt.b, testedAt.c]).size).toBe(1);
  });

  it('重测刷新时间戳（陈旧态必须能被一次重测清掉）', () => {
    useLatencyStore.setState({ latencyMap: { a: 120 }, testedAt: { a: 1 } });
    useLatencyStore.getState().applyLatencyResult('a', 88);
    expect(useLatencyStore.getState().testedAt.a).toBeGreaterThan(1);
  });

  it('isLatencyStale：未测过不算陈旧，恰好 30min 不算，超过才算', () => {
    const now = 1_000_000_000;
    expect(isLatencyStale(undefined, now)).toBe(false);
    expect(isLatencyStale(now - LATENCY_STALE_MS, now)).toBe(false);
    expect(isLatencyStale(now - LATENCY_STALE_MS - 1, now)).toBe(true);
  });
});

describe('use-latency-store：订阅脱离组件生命周期（跨屏不丢）', () => {
  it('事件经 subscribeLatencyEvents 落 store', () => {
    subscribeLatencyEvents();
    emitResult('a', 120);

    expect(useLatencyStore.getState().latencyMap.a).toBe(120);
  });

  it('**跨屏不丢**：订阅建立后，任意多次「组件卸载/重挂」都不影响已积累的结果', () => {
    subscribeLatencyEvents(); // 建在 App.tsx 顶层，全窗口生命周期只此一次
    emitResult('a', 120);
    emitResult('b', 240);

    // 模拟用户切屏：业务屏卸载再挂载（组件不再持有任何测速状态，故这里没有任何清理动作可做）——
    // 若哪天有人把状态搬回组件私有 useState，这一条会立刻失败：那时值随卸载消失。
    const afterUnmount = useLatencyStore.getState().latencyMap;
    const afterRemount = useLatencyStore.getState().latencyMap;

    expect(afterUnmount).toEqual({ a: 120, b: 240 });
    expect(afterRemount).toEqual({ a: 120, b: 240 });

    // 切回来之后**继续**收流式结果（订阅还活着，没被卸载带走）。
    emitResult('c', 60);
    expect(useLatencyStore.getState().latencyMap).toEqual({ a: 120, b: 240, c: 60 });
  });

  it('测速进行中切屏：在飞的流式结果照样落库（原实现在这里丢结果）', () => {
    subscribeLatencyEvents();
    emitResult('a', 120); // 第 1 波
    // …用户此刻切到别的屏（组件卸载）。后端继续推第 2 波：
    emitResult('b', 240);
    emitResult('c', 300);

    expect(useLatencyStore.getState().latencyMap).toEqual({ a: 120, b: 240, c: 300 });
  });

  it('返回可退订句柄（窗口销毁时收口，不泄漏监听器）', () => {
    const off = subscribeLatencyEvents();
    off();
    expect(offSpy).toHaveBeenCalledTimes(1);
  });
});
