/**
 * `switchServer` 回归测（钉死「静默直连」根因）。
 *
 * **根因**：`switchServer` 曾只更新扁平 `selectedServerId`，不同步 `config.selectedServerId`。
 * 而 `startProxy` 发给后端的是**整个 `config`** → 起核用的是 config 里的旧值（常见为 `__direct__`）
 * → 后端按旧值生成 sing-box config，selector default 落 `direct` = **明文直连**；
 * 而 UI 因扁平字段已更新，照常显示新节点 + 「已连接」绿灯 → 用户以为走代理，实则未加密。
 *
 * 故本文件的不变式只有一条、但必须双写都验：**切完节点，两处 selectedServerId 必须同时是新值**。
 * 只验其一都会漏掉本 bug（漏验 config 那处 = 正是原 bug；漏验扁平那处 = UI 不跟手）。
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';

// api 必须在 import store 之前 mock（store 模块顶层 import 了它）。
const switchMock = vi.fn(async (_id: string) => undefined);
vi.mock('../ipc', () => ({
  api: {
    server: { switch: (id: string) => switchMock(id) },
    config: { get: vi.fn(), save: vi.fn() },
    proxy: { getStatus: vi.fn() },
  },
}));

import { useAppStore } from './app-store';

/** 起核前的形态：config 里存着旧的选中值（这里用直连哨兵 = 真机命中的那一版）。 */
function seed(oldSelected: string) {
  useAppStore.setState({
    selectedServerId: oldSelected,
    config: {
      selectedServerId: oldSelected,
      servers: [],
      proxyMode: 'smart',
    } as never,
  });
}

describe('app-store switchServer', () => {
  beforeEach(() => {
    switchMock.mockClear();
  });

  it('切节点后：扁平 selectedServerId 与 config.selectedServerId 必须都更新', async () => {
    seed('__direct__');

    await useAppStore.getState().switchServer('n-hk');

    const s = useAppStore.getState();
    expect(s.selectedServerId).toBe('n-hk');
    // ↓ 这一条就是本 bug 的根因断言：漏了它，起核就会按 `__direct__` 落直连。
    expect(s.config?.selectedServerId).toBe('n-hk');
  });

  it('config 里绝不残留旧的直连哨兵（起核按它走会落明文直连）', async () => {
    seed('__direct__');

    await useAppStore.getState().switchServer('n-hk');

    expect(useAppStore.getState().config?.selectedServerId).not.toBe('__direct__');
  });

  it('节点间切换同样双写（不只是「从直连切出去」这一种）', async () => {
    seed('n-jp');

    await useAppStore.getState().switchServer('n-hk');

    const s = useAppStore.getState();
    expect(s.selectedServerId).toBe('n-hk');
    expect(s.config?.selectedServerId).toBe('n-hk');
  });

  it('真的落到了后端（不是只改本地 store）', async () => {
    seed('__direct__');

    await useAppStore.getState().switchServer('n-hk');

    expect(switchMock).toHaveBeenCalledTimes(1);
    expect(switchMock).toHaveBeenCalledWith('n-hk');
  });

  it('config 尚未加载（null）时不得凭空造出 config，但扁平字段照常更新', async () => {
    useAppStore.setState({ selectedServerId: null, config: null });

    await useAppStore.getState().switchServer('n-hk');

    const s = useAppStore.getState();
    expect(s.selectedServerId).toBe('n-hk');
    expect(s.config).toBeNull();
  });
});
