/**
 * 提交计划纯逻辑单测（vitest，node 环境）。
 * 守的是「登录成果孤儿化」那条 bug：新建路径必须带真实 id 落盘后再登录。
 */
import { describe, expect, it } from 'vitest';
import type { ServerConfig } from '@/contracts/types';
import { planTsLoginSubmit } from './ts-login-server';

const MINTED = 'minted-id-1';
const mint = () => MINTED;

function tsNode(overrides: Partial<ServerConfig> = {}): ServerConfig {
  return {
    id: 'ts-existing',
    name: 'Tailscale',
    protocol: 'tailscale',
    address: '',
    port: 0,
    tailscaleSettings: {},
    ...overrides,
  } as ServerConfig;
}

describe('planTsLoginSubmit —— 新建路径', () => {
  it('无既有节点 → 带 mint 的真实 id，persist=add（绝不发空串 id）', () => {
    const { server, persist } = planTsLoginSubmit(undefined, 'browser', '', mint);
    expect(server.id).toBe(MINTED);
    expect(server.id).not.toBe('');
    expect(persist).toBe('add');
    expect(server.protocol).toBe('tailscale');
  });

  it('authkey 模式新建 → key 落进 tailscaleSettings 一并写盘（不再只发给登录核）', () => {
    const { server, persist } = planTsLoginSubmit(undefined, 'authkey', '  tskey-auth-abc  ', mint);
    expect(server.tailscaleSettings?.authKey).toBe('tskey-auth-abc');
    expect(persist).toBe('add');
  });

  it('默认设置留空 → 不覆写后端缺省（allowInternet / alwaysRouteSubnets 缺省即 true）', () => {
    const { server } = planTsLoginSubmit(undefined, 'browser', '', mint);
    expect(server.tailscaleSettings).toEqual({});
  });
});

describe('planTsLoginSubmit —— 既有节点路径', () => {
  it('browser 模式 → 复用既有 id，persist=none（不写盘、不触发 CONFIG_CHANGED）', () => {
    const { server, persist } = planTsLoginSubmit(tsNode(), 'browser', '', mint);
    expect(server.id).toBe('ts-existing');
    expect(persist).toBe('none');
  });

  it('authkey 变更 → persist=update（key 必须落盘，否则 UI 报成功而配置里没有）', () => {
    const { server, persist } = planTsLoginSubmit(
      tsNode({ tailscaleSettings: { authKey: 'old' } }),
      'authkey',
      'new-key',
      mint
    );
    expect(server.tailscaleSettings?.authKey).toBe('new-key');
    expect(persist).toBe('update');
  });

  it('authkey 未变 → persist=none（省一次无谓写盘/重启）', () => {
    const { persist } = planTsLoginSubmit(
      tsNode({ tailscaleSettings: { authKey: 'same' } }),
      'authkey',
      '  same  ',
      mint
    );
    expect(persist).toBe('none');
  });

  it('绝不 mutate 既有节点（app-store.servers 的 live 引用）', () => {
    const existing = tsNode({ tailscaleSettings: { authKey: 'old' } });
    const { server } = planTsLoginSubmit(existing, 'authkey', 'new-key', mint);
    expect(existing.tailscaleSettings?.authKey).toBe('old');
    expect(server.tailscaleSettings).not.toBe(existing.tailscaleSettings);
  });
});
