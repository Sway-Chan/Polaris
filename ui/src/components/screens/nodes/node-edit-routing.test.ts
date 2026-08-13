/**
 * 节点编辑入口分流单测（vitest，node 环境）。
 *
 * 钉住的是一条**数据损坏防线**：wireguard/tailscale 节点若被路由到通用 NodeDialog，保存时协议会
 * 回落 vless 并丢弃 wireguardSettings/tailscaleSettings（真机确认过的路径）。
 */

import { describe, expect, it } from 'vitest';
import type { ServerConfig } from '@/contracts/types';
import { editDialogFor } from './node-edit-routing';

type Protocol = ServerConfig['protocol'];

function server(patch: Partial<ServerConfig>): ServerConfig {
  return { id: 'id-1', name: 'n', protocol: 'vless', address: 'a.example', port: 443, ...patch } as ServerConfig;
}

describe('editDialogFor', () => {
  it('普通代理协议 → 通用 NodeDialog，带 serverId 预填', () => {
    const protos: Protocol[] = ['vless', 'vmess', 'trojan', 'shadowsocks', 'hysteria2', 'tuic', 'ssh', 'custom'];
    for (const protocol of protos) {
      expect(editDialogFor(server({ protocol, id: `id-${protocol}` }))).toEqual({
        kind: 'node',
        serverId: `id-${protocol}`,
      });
    }
  });

  it('WireGuard → WgDialog（携 id，可多实例）', () => {
    expect(editDialogFor(server({ protocol: 'wireguard', id: 'wg-1' }))).toEqual({
      kind: 'wg',
      serverId: 'wg-1',
    });
  });

  it('Tailscale → TsSettingsDialog（单例，不携 id）', () => {
    expect(editDialogFor(server({ protocol: 'tailscale', id: 'ts-1' }))).toEqual({ kind: 'ts-settings' });
  });

  // WARP 的 protocol 也是 wireguard —— 判定顺序错了就会被当普通 WG 节点丢进 WgDialog。
  it('WARP（warpDevice 标记）先于 WireGuard 命中 → warp 单例槽编辑态', () => {
    const warp = server({
      protocol: 'wireguard',
      id: 'warp-1',
      wireguardSettings: {
        privateKey: 'k',
        localAddress: ['172.16.0.2/32'],
        peerPublicKey: 'p',
        warpDevice: { deviceId: 'd', token: 't' },
      },
    });
    expect(editDialogFor(warp)).toEqual({ kind: 'warp', edit: true });
  });

  it('WARP（endpoint 域名特征）同样先于 WireGuard 命中', () => {
    expect(editDialogFor(server({ protocol: 'wireguard', id: 'warp-2', address: 'engage.cloudflareclient.com' }))).toEqual({
      kind: 'warp',
      edit: true,
    });
  });
});
