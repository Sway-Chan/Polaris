/**
 * A4：「按延迟排序」偏好的**跨窗同步**。
 *
 * 被修的缺陷：localStorage 的**存储**跨窗共享，但每个 webview 各有一份 zustand store 实例，而 store
 * 只在创建时读一次。托盘浮层按需创建后会以 warm WebView 保留至 120s 回收；这期间用户在主窗打开
 * 「按延迟排序」，主窗和 localStorage 已是新值，现存浮层 store 却不会自行重读。不能靠等回收来碰巧修正。
 *
 * 故这里锁两件事：判定本身（哪些事件该回灌、回灌成什么值），以及「回灌腿不得自己再写一次 localStorage」
 * ——那会把刚从别处收到的值再写回去，形成回声。
 */
import { describe, it, expect, beforeEach, vi } from 'vitest';
import { useNodeSortStore, applyNodeSortStorageEvent } from './use-node-sort-store';

const KEY = 'polaris.nodeSortByLatency';

beforeEach(() => {
  useNodeSortStore.setState({ sortByLatency: false });
});

describe('applyNodeSortStorageEvent —— 别的窗口改了偏好即回灌', () => {
  it('本 key 变 true → store 跟随', () => {
    expect(applyNodeSortStorageEvent({ key: KEY, newValue: 'true' })).toBe(true);
    expect(useNodeSortStore.getState().sortByLatency).toBe(true);
  });

  it('本 key 变 false → store 跟随', () => {
    useNodeSortStore.setState({ sortByLatency: true });
    expect(applyNodeSortStorageEvent({ key: KEY, newValue: 'false' })).toBe(true);
    expect(useNodeSortStore.getState().sortByLatency).toBe(false);
  });

  it('非法/缺失值按 false 处理（与 loadNodeSortByLatency 的 === "true" 同口径）', () => {
    useNodeSortStore.setState({ sortByLatency: true });
    applyNodeSortStorageEvent({ key: KEY, newValue: null });
    expect(useNodeSortStore.getState().sortByLatency).toBe(false);
  });

  it('别的 key 一律不理（本 store 不该被无关写入唤醒）', () => {
    expect(applyNodeSortStorageEvent({ key: 'polaris.language', newValue: 'zh-CN' })).toBe(false);
    expect(applyNodeSortStorageEvent({ key: 'polaris.sidebar.collapsed', newValue: '1' })).toBe(false);
    expect(useNodeSortStore.getState().sortByLatency).toBe(false);
  });

  it('key===null（storage.clear()）→ 回落默认按名称', () => {
    useNodeSortStore.setState({ sortByLatency: true });
    expect(applyNodeSortStorageEvent({ key: null, newValue: null })).toBe(true);
    expect(useNodeSortStore.getState().sortByLatency).toBe(false);
  });

  it('值未变 → 不动 store（省掉一整轮无谓重渲染）', () => {
    expect(applyNodeSortStorageEvent({ key: KEY, newValue: 'false' })).toBe(false);
  });

  it('回灌**不得**再写一次 localStorage（否则是自己触发自己的回声）', () => {
    const setItem = vi.fn();
    const prev = (globalThis as { localStorage?: unknown }).localStorage;
    Object.defineProperty(globalThis, 'localStorage', {
      value: { getItem: () => null, setItem },
      configurable: true,
    });
    try {
      applyNodeSortStorageEvent({ key: KEY, newValue: 'true' });
      // 走 setSortByLatency 就会 persist 一次 —— 这条断言把那个写法钉死。
      expect(setItem).not.toHaveBeenCalled();
    } finally {
      if (prev === undefined) {
        delete (globalThis as { localStorage?: unknown }).localStorage;
      } else {
        Object.defineProperty(globalThis, 'localStorage', { value: prev, configurable: true });
      }
    }
  });
});
