/**
 * 解锁检测显示态回归测（钉死「六徽章永久转圈」那一族根因的**前端半边**）。
 *
 * 后端半边（invalidate → 1500ms 去抖 → 自跑、整轮 10s deadline）在 `src-tauri/src/runtime/unlock.rs`
 * 的单测里锁。本文件锁的是前端三条：
 *
 * 1. **progress merge**：逐服务 settle 必须**合入** results，不是整体替换。原实现
 *    `setUnlock({ results: { [id]: r } })` 每收一条就抹掉另外五项 —— 六个徽章只有最后 settle 的那个留得住。
 * 2. **终态收口**：`applyUnlockSnapshot` 的三条分流（blocked / notReady / 真检测）都必须把 `running` 落回
 *    false。丢掉 `run()` 返回值 = 早退路径的 `running:true` 永远没人收口 = 转圈。
 * 3. **陈旧轮不覆盖**：后端丢弃轮返回的空快照不得把新一轮的「检测中」清成空 idle。
 */

import { describe, it, expect, beforeEach } from 'vitest';

import type { UnlockSnapshot } from '../contracts/unlock-detection';

// api 必须在 import store 之前 mock（store 模块顶层 import 了它）。
import { vi } from 'vitest';
vi.mock('../ipc', () => ({
  api: {
    server: { switch: vi.fn() },
    config: { get: vi.fn(), save: vi.fn() },
    proxy: { getStatus: vi.fn() },
  },
}));

import { useAppStore } from './app-store';

const snap = (over: Partial<UnlockSnapshot> = {}): UnlockSnapshot =>
  ({
    results: {},
    checkedAt: null,
    egress: null,
    ...over,
  }) as UnlockSnapshot;

beforeEach(() => {
  useAppStore.getState().resetUnlock();
});

describe('setUnlockProgress —— merge 语义', () => {
  /**
   * **变异锁**：把 `setUnlockProgress` 改回整体替换
   * （`results: { [serviceId]: result }`）→ 本测第二条断言（chatgpt 仍在）转红。
   * 这正是真机上「徽章逐个点亮又逐个消失、最后只剩一个」的形态。
   */
  it('逐服务 settle 合入 results，不抹掉先到的其它服务', () => {
    const s = useAppStore.getState();
    s.setUnlockProgress('chatgpt', { status: 'ok' });
    s.setUnlockProgress('netflix', { status: 'partial', region: 'HK' });
    s.setUnlockProgress('disney', { status: 'timeout' });

    const { results } = useAppStore.getState().unlock;
    expect(Object.keys(results).sort()).toEqual(['chatgpt', 'disney', 'netflix']);
    expect(results.chatgpt.status).toBe('ok');
    expect(results.netflix).toEqual({ status: 'partial', region: 'HK' });
  });

  it('同一服务再次 settle 覆盖自身，不影响他人', () => {
    const s = useAppStore.getState();
    s.setUnlockProgress('netflix', { status: 'timeout' });
    s.setUnlockProgress('chatgpt', { status: 'ok' });
    s.setUnlockProgress('netflix', { status: 'ok', region: 'JP' }); // settle-retry 补测点亮

    const { results } = useAppStore.getState().unlock;
    expect(results.netflix).toEqual({ status: 'ok', region: 'JP' });
    expect(results.chatgpt.status).toBe('ok');
  });

  it('progress 不动 running —— 检测中途仍是检测中', () => {
    useAppStore.getState().beginUnlockCheck();
    useAppStore.getState().setUnlockProgress('chatgpt', { status: 'ok' });
    expect(useAppStore.getState().unlock.running).toBe(true);
  });
});

describe('applyUnlockSnapshot —— 终态必须收口 running', () => {
  /**
   * **变异锁**（这三条是同一族）：任一分支里漏写 `running: false` → 对应断言转红。
   * 真机症状 = 点了刷新，后端毫秒级早退，前端六个徽章却一直转圈。
   */
  it('gating 短路（blockedReason）→ idle，且不打 lastRunAt（毫秒级短路不该触发 15s 冷却）', () => {
    useAppStore.getState().beginUnlockCheck();
    useAppStore.getState().applyUnlockSnapshot(snap({ blockedReason: 'proxy-not-running' }));

    const u = useAppStore.getState().unlock;
    expect(u.running).toBe(false);
    expect(u.results).toEqual({});
    expect(u.lastRunAt).toBeNull();
  });

  it('notReady（就绪门未过 / 整轮预算耗尽）→ idle 但打戳（后端已置 lastRunAt，冷却须镜像）', () => {
    useAppStore.getState().beginUnlockCheck();
    useAppStore.getState().applyUnlockSnapshot(snap({ notReady: true }));

    const u = useAppStore.getState().unlock;
    expect(u.running).toBe(false);
    expect(u.checkedAt).toBeNull();
    expect(u.lastRunAt).not.toBeNull();
  });

  it('真检测落定 → 落 results/checkedAt/egress，且 lastRunAt 取 checkedAt 而非 Date.now()', () => {
    useAppStore.getState().beginUnlockCheck();
    const checkedAt = 1_700_000_000_000;
    useAppStore.getState().applyUnlockSnapshot(
      snap({
        results: { chatgpt: { status: 'ok' }, netflix: { status: 'partial' } },
        checkedAt,
        egress: { ip: '1.1.1.1', region: 'US' },
      })
    );

    const u = useAppStore.getState().unlock;
    expect(u.running).toBe(false);
    expect(u.checkedAt).toBe(checkedAt);
    expect(u.egress).toEqual({ ip: '1.1.1.1', region: 'US' });
    // 取 checkedAt：TTL 命中回放的是旧 checkedAt，用 Date.now() 会把 15s 前的旧检测误算成刚跑完 → 误禁刷新钮。
    expect(u.lastRunAt).toBe(checkedAt);
  });

  /**
   * **变异锁**：删掉 `applyUnlockSnapshot` 开头的陈旧轮 no-op 早退 → 本测转红。
   * 场景：后端归属校验丢弃一轮（返回空快照）时，新一轮已 beginUnlockCheck 接管显示；
   * 若让空快照落地，会把新一轮的「检测中」清成空 idle（徽章闪一下变灰）。
   */
  it('陈旧轮空快照不覆盖新一轮的「检测中」', () => {
    useAppStore.getState().beginUnlockCheck();
    useAppStore.getState().applyUnlockSnapshot(snap()); // 丢弃轮：空 results + 无任何终态标记

    expect(useAppStore.getState().unlock.running).toBe(true);
  });
});

describe('beginUnlockCheck / resetUnlock', () => {
  it('beginUnlockCheck 保留 lastRunAt —— 冷却窗由上次**完成**时刻派生，不因新一轮开跑而重置', () => {
    useAppStore.getState().applyUnlockSnapshot(
      snap({ results: { chatgpt: { status: 'ok' } }, checkedAt: 12_345 })
    );
    useAppStore.getState().beginUnlockCheck();

    const u = useAppStore.getState().unlock;
    expect(u.running).toBe(true);
    expect(u.results).toEqual({});
    expect(u.lastRunAt).toBe(12_345);
  });

  it('resetUnlock 连 lastRunAt 一并清（重连后不被陈旧冷却误锁刷新钮）', () => {
    useAppStore.getState().applyUnlockSnapshot(
      snap({ results: { chatgpt: { status: 'ok' } }, checkedAt: 12_345 })
    );
    useAppStore.getState().resetUnlock();

    expect(useAppStore.getState().unlock).toEqual({
      results: {},
      running: false,
      checkedAt: null,
      egress: null,
      lastRunAt: null,
    });
  });
});
