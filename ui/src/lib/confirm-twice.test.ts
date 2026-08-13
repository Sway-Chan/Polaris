/**
 * `createConfirmTwice` 单测 —— 原型 confirmTwice（L3211-3218）四条语义的直测。
 *
 * 测的是**纯核心**而不是 hook：本仓 vitest `environment:'node'`（无 jsdom / testing-library，有意为之），
 * hook 渲染不了。核心零 React 依赖 + 假时钟 ⇒ 超时与状态机可逐条钉死。
 * 「hook 真的用了这个核心、各屏真的用了这个 hook」由 `destructive-confirm-wiring.test.ts` 的源码守卫管。
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createConfirmTwice, CONFIRM_TWICE_MS } from './confirm-twice';

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

/** 收集 onChange 通知，便于断言「武装/复位」的完整序列而不只是末态。 */
function harness(timeoutMs?: number) {
  const changes: (string | null)[] = [];
  const core = createConfirmTwice((a) => changes.push(a), timeoutMs);
  return { changes, core };
}

describe('原型 L3217：超时常量逐字对齐', () => {
  it('CONFIRM_TWICE_MS === 2600', () => {
    // 变异对照：改成 2000/3000 本条即红。这是与原型的唯一数字锚点，不许「差不多」。
    expect(CONFIRM_TWICE_MS).toBe(2600);
  });
});

describe('原型 L3213-3216：首次武装、二次执行', () => {
  it('首次点击只武装、**不执行** action', () => {
    const { core, changes } = harness();
    const action = vi.fn();
    core.confirmTwice('k', action);
    expect(action, '首次点击就执行 = 二次确认形同虚设').not.toHaveBeenCalled();
    expect(changes).toEqual(['k']);
  });

  it('同一 key 第二次点击执行 action，且先复位再执行', () => {
    const { core, changes } = harness();
    const seen: (string | null)[] = [];
    // action 里读到的必须已经是复位后的状态（原型顺序：复位 → action()）。
    const action = vi.fn(() => seen.push(changes[changes.length - 1]));
    core.confirmTwice('k', action);
    core.confirmTwice('k', action);
    expect(action).toHaveBeenCalledTimes(1);
    expect(changes).toEqual(['k', null]);
    expect(seen, 'action 执行前必须已复位（否则按钮停在红色确认态）').toEqual([null]);
  });

  it('执行后回到未武装态：再点一次只是重新武装，不会连着删两次', () => {
    const { core } = harness();
    const action = vi.fn();
    core.confirmTwice('k', action);
    core.confirmTwice('k', action); // 执行第 1 次
    core.confirmTwice('k', action); // 重新武装
    expect(action, '第三次点击必须只是重新武装').toHaveBeenCalledTimes(1);
    core.confirmTwice('k', action);
    expect(action).toHaveBeenCalledTimes(2);
  });
});

describe('原型 L3217：2600ms 自动复位', () => {
  it('2600ms 到点自动复位；此后再点只是重新武装', () => {
    const { core, changes } = harness();
    const action = vi.fn();
    core.confirmTwice('k', action);
    vi.advanceTimersByTime(CONFIRM_TWICE_MS);
    expect(changes).toEqual(['k', null]);
    core.confirmTwice('k', action);
    // 变异对照：删掉 setTimeout 那段 → 这里 action 会被立刻调用（因为 armed 还停在 'k'），本条转红。
    expect(action, '超时复位后再点必须重新走第一段').not.toHaveBeenCalled();
  });

  it('差 1ms 未到点仍处于武装态（边界不是「大约 2.6 秒」）', () => {
    const { core, changes } = harness();
    const action = vi.fn();
    core.confirmTwice('k', action);
    vi.advanceTimersByTime(CONFIRM_TWICE_MS - 1);
    expect(changes).toEqual(['k']);
    core.confirmTwice('k', action);
    expect(action).toHaveBeenCalledTimes(1);
  });
});

describe('原型 L3213：第二次点击**先清 timeout**', () => {
  it('确认执行后，原定时器不得再打一发复位通知', () => {
    const { core, changes } = harness();
    core.confirmTwice('k', () => undefined);
    core.confirmTwice('k', () => undefined);
    expect(changes).toEqual(['k', null]);
    vi.advanceTimersByTime(CONFIRM_TWICE_MS * 2);
    // 变异对照：把 `clear()` 从确认腿删掉 → 这里会多出一发 null（在已复位/已卸载的组件上 setState）。
    expect(changes, '旧定时器没清：确认后仍会补一发复位').toEqual(['k', null]);
  });

  it('换 key 武装时也清旧定时器（旧的那发不许把新武装态踹掉）', () => {
    const { core, changes } = harness();
    const a = vi.fn();
    core.confirmTwice('a', a);
    vi.advanceTimersByTime(CONFIRM_TWICE_MS - 100); // 旧定时器还剩 100ms
    core.confirmTwice('b', a);
    vi.advanceTimersByTime(200); // 若旧定时器没清，此刻会复位掉刚武装的 'b'
    expect(changes, "旧定时器把新武装的 'b' 踹掉了").toEqual(['a', 'b']);
    core.confirmTwice('b', a);
    expect(a, "'b' 应仍处武装态，第二点即执行").toHaveBeenCalledTimes(1);
  });
});

describe('单槽语义：同时只有一个待确认项', () => {
  it('武装 B 会解除 A —— A 上的「第二次点击」退回第一段，不会误删', () => {
    const { core } = harness();
    const delA = vi.fn();
    const delB = vi.fn();
    core.confirmTwice('a', delA);
    core.confirmTwice('b', delB);
    core.confirmTwice('a', delA);
    // 方向安全：收紧后只会少删，不会多删。
    expect(delA, "A 已被 B 解除武装，这一点只能是重新武装").not.toHaveBeenCalled();
    expect(delB).not.toHaveBeenCalled();
  });
});

describe('dispose：卸载清定时器', () => {
  it('dispose 后定时器不再打出复位通知（React 侧防「已卸载组件 setState」）', () => {
    const { core, changes } = harness();
    core.confirmTwice('k', () => undefined);
    core.dispose();
    vi.advanceTimersByTime(CONFIRM_TWICE_MS * 2);
    // 变异对照：把 useEffect 卸载清理删掉 / 让 dispose 变成空函数 → 这里会多出一发 null。
    expect(changes).toEqual(['k']);
  });

  it('未武装时 dispose 无副作用（可重复调用）', () => {
    const { core, changes } = harness();
    core.dispose();
    core.dispose();
    expect(changes).toEqual([]);
  });
});

describe('自定义超时（工厂第二参）不改变其余语义', () => {
  it('注入 10ms 时按注入值复位（证明 2600 不是硬编码在逻辑里）', () => {
    const { core, changes } = harness(10);
    core.confirmTwice('k', () => undefined);
    vi.advanceTimersByTime(9);
    expect(changes).toEqual(['k']);
    vi.advanceTimersByTime(1);
    expect(changes).toEqual(['k', null]);
  });
});

describe('reset：点别处即复原（陈先生 2026-07-30）', () => {
  it('武装后 reset → 立刻复位，且原定时器不再补一发 null', () => {
    const { core, changes } = harness();
    core.confirmTwice('k', () => undefined);
    core.reset();
    expect(changes).toEqual(['k', null]);
    // 变异对照：reset 里漏掉 clear() → 定时器仍在，这里会多出第二个 null。
    vi.advanceTimersByTime(CONFIRM_TWICE_MS * 2);
    expect(changes).toEqual(['k', null]);
  });

  it('reset 后再点同一 key 是**重新武装**而非执行（复位必须真的解除待定态）', () => {
    const { core } = harness();
    const del = vi.fn();
    core.confirmTwice('k', del);
    core.reset();
    core.confirmTwice('k', del);
    expect(del, 'reset 没真解除的话这一点就直接删了').not.toHaveBeenCalled();
    core.confirmTwice('k', del);
    expect(del).toHaveBeenCalledTimes(1);
  });

  it('未武装时 reset 幂等（不空发通知 → 不触发无谓重渲）', () => {
    const { core, changes } = harness();
    core.reset();
    core.reset();
    expect(changes).toEqual([]);
  });
});
