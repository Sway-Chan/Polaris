/**
 * 组网单例槽闸门（`meshSingletonConflict` / `admitMeshSingletons`）单测。
 *
 * 守的是这条真机缺陷的**根因**：接入区卡片只控入口显隐，而 WgDialog（粘贴 Cloudflare `.conf`）、
 * ImportDialog（批量入库）、NodeDialog、克隆 都能绕开它直调 `server:add` / `server:addBulk`，
 * 后端两命令均无守卫 → 造出第二个 WARP → 与主 TUN 抢内核 utun → `Connect: resource busy` FATAL。
 *
 * 逃逸面按「WARP 怎么被认出来」穷举：`warpDevice` 标记（新节点）、端点域名兜底（旧/导入节点，
 * 正是 `.conf` 那条腿）、大小写、子域；再叠上 editingId 放行与批量内自增占槽两条。
 */
import { describe, it, expect } from 'vitest';
import type { ServerConfig } from '../contracts/types';
import { admitMeshSingletons, meshSingletonConflict } from './endpoint-routes';

const srv = (p: Partial<ServerConfig> & { id: string }): ServerConfig =>
  ({ name: p.id, protocol: 'vless', address: '', port: 443, ...p }) as ServerConfig;

/** WireGuardSettings 的必填三件套（本组测试只关心 warpDevice / address，其余给占位值）。 */
const wgBase = { privateKey: 'k', localAddress: ['10.0.0.2/32'], peerPublicKey: 'p' };

/** 带自删凭据的新 WARP 节点。 */
const warpTagged = (id: string): ServerConfig =>
  srv({
    id,
    protocol: 'wireguard',
    address: '162.159.192.1',
    wireguardSettings: { ...wgBase, warpDevice: { deviceId: 'd', token: 't' } },
  });

/** 旧/导入的 WARP：无 warpDevice，只有端点域名可认（wg-quick `.conf` 那条腿的真实形态）。 */
const warpByDomain = (id: string, host = 'engage.cloudflareclient.com'): ServerConfig =>
  srv({ id, protocol: 'wireguard', address: host, wireguardSettings: { ...wgBase } });

const tsNode = (id: string): ServerConfig => srv({ id, protocol: 'tailscale' });
const plainWg = (id: string): ServerConfig =>
  srv({ id, protocol: 'wireguard', address: '203.0.113.7', wireguardSettings: { ...wgBase } });

describe('meshSingletonConflict —— 槽位空闲时一律放行', () => {
  it('空节点集：WARP / TS / 普通 WG 都放行', () => {
    expect(meshSingletonConflict(warpTagged('new'), [])).toBeNull();
    expect(meshSingletonConflict(tsNode('new'), [])).toBeNull();
    expect(meshSingletonConflict(plainWg('new'), [])).toBeNull();
  });

  it('已有普通 WireGuard 不占 WARP 槽（WARP 单例 ≠ WireGuard 单例）', () => {
    expect(meshSingletonConflict(warpTagged('new'), [plainWg('a'), plainWg('b')])).toBeNull();
  });

  it('已有 WARP 不挡普通 WireGuard / 代理节点', () => {
    const existing = [warpTagged('w1')];
    expect(meshSingletonConflict(plainWg('new'), existing)).toBeNull();
    expect(meshSingletonConflict(srv({ id: 'new' }), existing)).toBeNull();
  });
});

describe('meshSingletonConflict —— WARP 槽被占（逐条逃逸面）', () => {
  it('已有带 warpDevice 的 WARP → 再加一个被拦', () => {
    expect(meshSingletonConflict(warpTagged('new'), [warpTagged('w1')])).toBe('warp');
  });

  it('**存量按域名认**：已有无 warpDevice 的旧 WARP，新 WARP 一样被拦', () => {
    expect(meshSingletonConflict(warpTagged('new'), [warpByDomain('w1')])).toBe('warp');
  });

  it('**候选按域名认**：粘贴 Cloudflare .conf（无 warpDevice）撞已注册 WARP 被拦（本次修复的真机路径）', () => {
    expect(meshSingletonConflict(warpByDomain('new'), [warpTagged('w1')])).toBe('warp');
  });

  it('域名判定大小写不敏感 + 认子域', () => {
    expect(meshSingletonConflict(warpByDomain('new', 'ENGAGE.CloudflareClient.COM'), [warpTagged('w1')])).toBe('warp');
    expect(meshSingletonConflict(warpByDomain('new', 'zero-trust.cloudflareclient.com'), [warpTagged('w1')])).toBe('warp');
  });

  it('protocol 大小写不敏感（导入产物可能是 "WireGuard"）', () => {
    const upper = { ...warpTagged('new'), protocol: 'WireGuard' } as unknown as ServerConfig;
    expect(meshSingletonConflict(upper, [warpTagged('w1')])).toBe('warp');
  });

  it('非 wireguard 协议即使地址含 cloudflareclient.com 也不算 WARP（不误拦代理节点）', () => {
    const vless = srv({ id: 'new', protocol: 'vless', address: 'a.cloudflareclient.com' });
    expect(meshSingletonConflict(vless, [warpTagged('w1')])).toBeNull();
  });
});

describe('meshSingletonConflict —— Tailscale 槽', () => {
  it('已有 TS → 再加一个被拦', () => {
    expect(meshSingletonConflict(tsNode('new'), [tsNode('t1')])).toBe('tailscale');
  });

  it('大小写不敏感', () => {
    const upper = { ...tsNode('new'), protocol: 'Tailscale' } as unknown as ServerConfig;
    expect(meshSingletonConflict(upper, [tsNode('t1')])).toBe('tailscale');
  });
});

describe('meshSingletonConflict —— editingId 放行自身', () => {
  it('编辑现有 WARP 节点不算「再加一个」', () => {
    expect(meshSingletonConflict(warpTagged('w1'), [warpTagged('w1')], 'w1')).toBeNull();
  });

  it('编辑现有 TS 节点不算「再加一个」', () => {
    expect(meshSingletonConflict(tsNode('t1'), [tsNode('t1')], 't1')).toBeNull();
  });

  it('editingId 只排除自身：改另一个 WG 节点的地址成 WARP 域名，仍撞已注册 WARP', () => {
    // WgDialog 编辑腿的真实场景——editingId 是被编辑的普通 WG 节点，槽位仍被 w1 占着。
    expect(
      meshSingletonConflict(warpByDomain('g1'), [warpTagged('w1'), plainWg('g1')], 'g1')
    ).toBe('warp');
  });
});

describe('admitMeshSingletons —— 批量导入逐条准入', () => {
  it('无冲突：全量准入，零拒收', () => {
    const nodes = [plainWg('a'), srv({ id: 'b' })];
    const { admitted, rejected } = admitMeshSingletons(nodes, []);
    expect(admitted).toEqual(nodes);
    expect(rejected).toEqual([]);
  });

  it('**准入者即刻占槽**：同一批里的两个 WARP 只进第一个（槽位空闲快照不得被复用）', () => {
    const { admitted, rejected } = admitMeshSingletons(
      [warpTagged('n1'), warpByDomain('n2'), plainWg('n3')],
      []
    );
    expect(admitted.map((s) => s.id)).toEqual(['n1', 'n3']);
    expect(rejected.map((s) => s.id)).toEqual(['n2']);
  });

  it('槽位已被存量占用 → 该条被拒，同批其余节点照常入库（不整批拒绝）', () => {
    const { admitted, rejected } = admitMeshSingletons(
      [plainWg('n1'), warpTagged('n2'), tsNode('n3'), srv({ id: 'n4' })],
      [warpTagged('w1')]
    );
    expect(admitted.map((s) => s.id)).toEqual(['n1', 'n3', 'n4']);
    expect(rejected.map((s) => s.id)).toEqual(['n2']);
  });

  it('两个槽各自独立计数（WARP 满、TS 空 → TS 仍可进一个，第二个被拒）', () => {
    const { admitted, rejected } = admitMeshSingletons(
      [tsNode('n1'), tsNode('n2'), warpTagged('n3')],
      [warpTagged('w1')]
    );
    expect(admitted.map((s) => s.id)).toEqual(['n1']);
    expect(rejected.map((s) => s.id)).toEqual(['n2', 'n3']);
  });

  it('不 mutate 入参（existing 数组与候选数组原样）', () => {
    const existing = [warpTagged('w1')];
    const candidates = [warpTagged('n1'), plainWg('n2')];
    admitMeshSingletons(candidates, existing);
    expect(existing).toHaveLength(1);
    expect(candidates).toHaveLength(2);
  });
});
