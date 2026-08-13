/**
 * 组网同网段「被覆盖（shadowed）」检测单测（`meshShadowedCidrs`，供节点卡角标）。
 *
 * 不变量来自 route-builder 的 `claimedCidrs`：一条 ip_cidr 只能指向一个 outbound，**首声明者占有**，
 * 排在后面的同段静默失效。角标存在的理由就是把这份静默显性化，故这里逐条锁：
 * 谁被判为被覆盖、抢占者是谁（tooltip 要答「被谁覆盖」）、以及 catch-all / tailnet 自动段这些
 * 「不该算冲突」与「该算冲突」的边界。
 */
import { describe, it, expect } from 'vitest';
import type { ServerConfig } from '../contracts/types';
import {
  collectRuleTargetedServerIds,
  meshForceRoutedServers,
  meshShadowedCidrs,
  TAILNET_CGNAT,
  TAILNET_ULA_V6,
} from './endpoint-routes';

const wg = (id: string, allowedIPs: string[], extra: Record<string, unknown> = {}): ServerConfig =>
  ({
    id,
    name: id,
    protocol: 'wireguard',
    address: '203.0.113.7',
    port: 51820,
    wireguardSettings: { allowedIPs, ...extra },
  }) as ServerConfig;

describe('meshShadowedCidrs —— 首声明者占有', () => {
  it('无重叠 → 空表', () => {
    const out = meshShadowedCidrs([wg('a', ['10.0.0.0/24']), wg('b', ['10.0.1.0/24'])]);
    expect(out.size).toBe(0);
  });

  it('同段重复：只有后来者被标记，且带出抢占者 id', () => {
    const out = meshShadowedCidrs([wg('a', ['10.0.0.0/24']), wg('b', ['10.0.0.0/24'])]);
    expect(out.has('a')).toBe(false);
    expect(out.get('b')).toEqual([{ cidr: '10.0.0.0/24', byId: 'a' }]);
  });

  it('三方争同一段：第二、三个都被标记，抢占者都是第一个', () => {
    const out = meshShadowedCidrs([
      wg('a', ['10.0.0.0/24']),
      wg('b', ['10.0.0.0/24']),
      wg('c', ['10.0.0.0/24']),
    ]);
    expect(out.get('b')).toEqual([{ cidr: '10.0.0.0/24', byId: 'a' }]);
    expect(out.get('c')).toEqual([{ cidr: '10.0.0.0/24', byId: 'a' }]);
  });

  it('部分重叠：只列真正被抢的段，各段抢占者可以不同', () => {
    const out = meshShadowedCidrs([
      wg('a', ['10.0.0.0/24']),
      wg('b', ['192.168.9.0/24']),
      wg('c', ['10.0.0.0/24', '192.168.9.0/24', '172.16.0.0/24']),
    ]);
    expect(out.get('c')).toEqual([
      { cidr: '10.0.0.0/24', byId: 'a' },
      { cidr: '192.168.9.0/24', byId: 'b' },
    ]);
    expect(out.has('a')).toBe(false);
    expect(out.has('b')).toBe(false);
  });

  it('顺序即语义：交换节点顺序，被标记的那个跟着换（对齐 route-builder 的首条命中）', () => {
    const a = wg('a', ['10.0.0.0/24']);
    const b = wg('b', ['10.0.0.0/24']);
    expect([...meshShadowedCidrs([a, b]).keys()]).toEqual(['b']);
    expect([...meshShadowedCidrs([b, a]).keys()]).toEqual(['a']);
  });

  it('catch-all（0.0.0.0/0 · ::/0）不参与占有——它是全隧道语义，由 selector/final 接管，不该虚报冲突', () => {
    const out = meshShadowedCidrs([
      wg('a', ['0.0.0.0/0', '::/0']),
      wg('b', ['0.0.0.0/0', '::/0']),
    ]);
    expect(out.size).toBe(0);
  });

  it('非 endpoint 协议无 force-route 段，不参与占有也不会被标记', () => {
    const vless = { id: 'p', name: 'p', protocol: 'vless', address: 'x', port: 443 } as ServerConfig;
    const out = meshShadowedCidrs([vless, wg('a', ['10.0.0.0/24']), wg('b', ['10.0.0.0/24'])]);
    expect(out.size).toBe(1);
    expect(out.get('b')).toEqual([{ cidr: '10.0.0.0/24', byId: 'a' }]);
  });

  it('两个 Tailscale 节点争 tailnet 自动段（100.64/10 + fd7a ULA）——单例闸门若被旁路，角标能看见', () => {
    const ts = (id: string): ServerConfig =>
      ({ id, name: id, protocol: 'tailscale', address: '', port: 0, tailscaleSettings: {} }) as ServerConfig;
    const out = meshShadowedCidrs([ts('t1'), ts('t2')]);
    expect(out.get('t2')).toEqual([
      { cidr: TAILNET_CGNAT, byId: 't1' },
      { cidr: TAILNET_ULA_V6, byId: 't1' },
    ]);
  });
});

describe('meshShadowedCidrs ∘ meshForceRoutedServers —— 与发射端同口径', () => {
  const engaged = wg('a', ['10.0.0.0/24']);
  // 「仅出网」：alwaysRouteSubnets=false → 未被选中/未被规则指向时本轮**不发射** force-route。
  const offMesh = wg('b', ['10.0.0.0/24'], { alwaysRouteSubnets: false });

  it('未 engaged 的「仅出网」节点被过滤掉 → 不虚报被覆盖', () => {
    const emitted = meshForceRoutedServers([engaged, offMesh], null, new Set());
    expect(emitted.map((s) => s.id)).toEqual(['a']);
    expect(meshShadowedCidrs(emitted).size).toBe(0);
  });

  it('该节点被选为主出口后即 engaged → 冲突如实显现', () => {
    const emitted = meshForceRoutedServers([engaged, offMesh], 'b', new Set());
    expect(meshShadowedCidrs(emitted).get('b')).toEqual([{ cidr: '10.0.0.0/24', byId: 'a' }]);
  });

  it('被启用规则显式指向亦算 engaged（口径经 collectRuleTargetedServerIds）', () => {
    const targeted = collectRuleTargetedServerIds([
      { enabled: true, action: 'proxy', targetServerId: 'b' },
      { enabled: false, action: 'proxy', targetServerId: 'zz' },
    ]);
    const emitted = meshForceRoutedServers([engaged, offMesh], null, targeted);
    expect(meshShadowedCidrs(emitted).get('b')).toEqual([{ cidr: '10.0.0.0/24', byId: 'a' }]);
  });
});
