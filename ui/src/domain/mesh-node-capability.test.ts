/**
 * 「组网资格由节点能力决定」这条判据的前端侧门。
 *
 * openconnect / openvpn-client 落 sing-box `endpoints[]`，但它们的可达网段由服务端在隧道建立后
 * push、配置期不可知 —— 所以它们**不是**组网协议。用户在 `meshRoutes` 里显式声明了段，该节点才
 * 具备组网路由能力：段被强制路由到它、节点卡开始渲染内网信息。节点页的 UI 分组是产品归属，
 * endpoint 腿始终在组网 Tab；不能再拿能力判据决定它保存后出现在哪。
 *
 * 判据镜像 Rust（`is_mesh_node` / `endpoint_forced_route_cidrs`），成员集一致性另由
 * `contracts/mesh-predicates-parity.test.ts` 与 Rust 源码对拍。
 */
import { describe, it, expect } from 'vitest';
import type { ServerConfig } from '@/contracts/types';
import {
  isMeshNode,
  isMeshProtocol,
  landsInEndpoints,
  endpointForcedRouteCidrs,
  meshAllowsInternet,
  wireguardPeerAllowedIps,
} from './endpoint-routes';
import { groupServersBySubscription } from './server-grouping';

const node = (over: Partial<ServerConfig>): ServerConfig =>
  ({ id: 'x', name: 'X', protocol: 'openconnect', address: 'v.example.com', port: 443, ...over }) as ServerConfig;

describe('endpoint 腿 VPN 客户端的组网资格', () => {
  it('声明了内网段才算组网节点；空白项不算声明', () => {
    for (const protocol of ['openconnect', 'openvpn-client'] as const) {
      expect(isMeshNode(node({ protocol }))).toBe(false);
      expect(isMeshNode(node({ protocol, meshRoutes: [] }))).toBe(false);
      expect(isMeshNode(node({ protocol, meshRoutes: ['   '] }))).toBe(false);
      expect(isMeshNode(node({ protocol, meshRoutes: ['10.10.0.0/16'] }))).toBe(true);
      // 协议级判据不受影响：它们永远不是组网**协议**，但永远是 endpoint 腿。
      expect(isMeshProtocol(protocol)).toBe(false);
      expect(landsInEndpoints(protocol)).toBe(true);
    }
  });

  it('组网协议与 meshRoutes 无关；普通出站协议塞了也不算', () => {
    expect(isMeshNode(node({ protocol: 'wireguard' }))).toBe(true);
    expect(isMeshNode(node({ protocol: 'tailscale' }))).toBe(true);
    expect(isMeshNode(node({ protocol: 'vless', meshRoutes: ['10.0.0.0/8'] }))).toBe(false);
  });

  it('声明的段进 force-route，catch-all 被剥掉（全隧道是另一个开关的事）', () => {
    expect(endpointForcedRouteCidrs(node({}))).toEqual([]);
    expect(
      endpointForcedRouteCidrs(node({ meshRoutes: ['10.10.0.0/16', '0.0.0.0/0', ' 192.168.1.0/24 '] }))
    ).toEqual(['10.10.0.0/16', '192.168.1.0/24']);
  });

  it('OpenVPN 的全隧道判据只在显式关掉时才为 false', () => {
    const ov = (redirect_gateway?: boolean) =>
      node({ protocol: 'openvpn-client', openvpnClientSettings: { redirect_gateway } } as Partial<ServerConfig>);
    // 缺省判 true：判 false 的后果是用户选了它作出口、流量却被兜底回 direct（静默走明文）。
    expect(meshAllowsInternet(ov(undefined))).toBe(true);
    expect(meshAllowsInternet(ov(true))).toBe(true);
    // 显式关 = 「只走声明的内网段，其余直连」。
    expect(meshAllowsInternet(ov(false))).toBe(false);
    // OpenConnect 无对应开关，本就是全隧道。
    expect(meshAllowsInternet(node({ protocol: 'openconnect' }))).toBe(true);
  });

  it('WARP 忽略旧配置的自定义路由字段，恒作为全隧道云出口', () => {
    const warp = node({
      protocol: 'wireguard',
      address: 'engage.cloudflareclient.com',
      port: 2408,
      wireguardSettings: {
        allowInternet: false,
        alwaysRouteSubnets: false,
        allowedIPs: ['10.0.0.0/24'],
      },
    } as Partial<ServerConfig>);
    expect(meshAllowsInternet(warp)).toBe(true);
    expect(endpointForcedRouteCidrs(warp)).toEqual([]);
    expect(wireguardPeerAllowedIps(warp)).toEqual(['0.0.0.0/0', '::/0']);
  });

  it('UI 分组与路由能力解耦：不论是否声明段，企业 VPN endpoint 都留在「组网」', () => {
    const bare = node({ id: 'a', protocol: 'openconnect' });
    const declared = node({ id: 'b', protocol: 'openvpn-client', meshRoutes: ['10.10.0.0/16'] });
    const groups = groupServersBySubscription([bare, declared], [], true);
    const manual = groups.find((g) => g.isManual);
    const mesh = groups.find((g) => g.isMesh);
    expect(manual?.servers.map((s) => s.id)).toEqual([]);
    expect(mesh?.servers.map((s) => s.id)).toEqual(['a', 'b']);
  });
});
