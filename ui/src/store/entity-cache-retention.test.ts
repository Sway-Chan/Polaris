import { beforeEach, describe, expect, it } from 'vitest';
import { useAppStore } from './app-store';
import { useSubscriptionProgressStore } from './use-subscription-progress-store';
import { useTailscaleLoginCacheStore } from './use-tailscale-login-cache-store';

describe('配置实体删除后的缓存统一回收', () => {
  beforeEach(() => {
    useAppStore.setState({
      tailscaleLoginStates: { keep: true, deleted: false },
      tailscaleAuthUrls: { keep: 'https://keep', deleted: 'https://deleted' },
      tailscaleLoginInitiated: { deleted: true },
      tailscaleStatuses: {
        deleted: {
          serverId: 'deleted',
          loggedIn: false,
          backendState: 'Stopped',
          expired: false,
          authURL: '',
          tailscaleIPs: [],
          peers: [],
          // Taildrop 四位在本用例无关，取「无能力、无文件」中性值。不给可选/默认是刻意的：
          // 契约加字段时这些夹具必须被人重新看一眼，而不是被 `?:` 静默补齐。
          canShareFiles: false,
          waitingFileCount: 0,
          receivingFileCount: 0,
          unreadFileCount: 0,
        },
      },
      invalidNodes: [
        { id: 'keep', tag: 'keep', reason: 'invalid' },
        { id: 'deleted', tag: 'deleted', reason: 'invalid' },
      ],
    });
    useTailscaleLoginCacheStore.setState({
      cache: {
        keep: { loggedIn: true, cachedAt: 1 },
        deleted: { loggedIn: false, cachedAt: 2 },
      },
    });
    useSubscriptionProgressStore.setState({
      progress: {
        deleted: {
          subscriptionId: 'deleted',
          phase: 'failed',
          error: 'failed',
        },
      },
    });
  });

  it('app-store 一次对账覆盖全部 per-server 状态', () => {
    useAppStore.getState().retainServerIds(['keep']);
    const state = useAppStore.getState();
    expect(state.tailscaleLoginStates).toEqual({ keep: true });
    expect(state.tailscaleAuthUrls).toEqual({ keep: 'https://keep' });
    expect(state.tailscaleLoginInitiated).toEqual({});
    expect(state.tailscaleStatuses).toEqual({});
    expect(state.invalidNodes).toEqual([{ id: 'keep', tag: 'keep', reason: 'invalid' }]);
  });

  it('持久登录缓存与订阅失败状态按各自配置实体回收', () => {
    useTailscaleLoginCacheStore.getState().retainServerIds(['keep']);
    useSubscriptionProgressStore.getState().retainSubscriptionIds([]);
    expect(useTailscaleLoginCacheStore.getState().cache).toEqual({
      keep: { loggedIn: true, cachedAt: 1 },
    });
    expect(useSubscriptionProgressStore.getState().progress).toEqual({});
  });
});
