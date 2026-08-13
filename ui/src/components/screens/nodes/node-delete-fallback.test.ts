/**
 * 删除节点兜底出口单测（vitest，node 环境）。
 *
 * 钉住的是一条**流量裸奔防线**：删当前选中节点时若兜底出口算错，用户以为还在代理、实则已直连。
 * 两类算错：
 *  1) 旧 bug：传被删节点自身 id → 后端 viable 校验恒假 → 落 DIRECT_SERVER_ID；
 *  2) #291：兜底候选未过可用性谓词 → 选中 subnet-only 组网节点（WG allowInternet:false / TS 无 exitNode）
 *     或配置不齐节点 → 后端热重设后公网静默走 direct。
 * 故本测同时覆盖「选剩余最快」与「谓词拒斥不可承载全隧道 / 配置不齐的候选」。
 */

import { describe, expect, it } from 'vitest';
import type { ServerConfig } from '@/contracts/types';
import { fallbackExitAfterDelete, isViableFallbackExit } from './node-delete-fallback';

/** 构造最小合法 ServerConfig：默认 vless + uuid（=可用出口，承载全隧道）。 */
function srv(id: string, over: Partial<ServerConfig> = {}): ServerConfig {
  return {
    id,
    name: id,
    protocol: 'vless',
    address: `${id}.example.net`,
    port: 443,
    uuid: `uuid-${id}`,
    ...over,
  };
}

/** 三个可用（full-tunnel）代理节点，保持原测 a/b/c 语义。 */
const servers: ServerConfig[] = [srv('a'), srv('b'), srv('c')];

/** subnet-only WireGuard（配置齐备但 allowInternet:false + 仅具体网段）→ 不承载全隧道，必须被拒。 */
const wgSubnetOnly = srv('wg', {
  protocol: 'wireguard',
  uuid: undefined,
  wireguardSettings: {
    privateKey: 'priv',
    peerPublicKey: 'pub',
    localAddress: ['10.0.0.2/32'],
    allowedIPs: ['192.168.1.0/24'],
    allowInternet: false,
  } as ServerConfig['wireguardSettings'],
});

/** Tailscale 无 exitNode → meshNodeCarriesFullTunnel=false，公网黑洞，必须被拒。 */
const tsNoExit = srv('ts', { protocol: 'tailscale', uuid: undefined, address: '', port: 0 });

/** 配置不齐的 vless（无 uuid）→ isServerComplete=false，必须被拒。 */
const incompleteVless = srv('bad', { uuid: undefined });

describe('isViableFallbackExit', () => {
  it('可用出口谓词：full-tunnel 代理节点通过，subnet-only 组网 / 配置不齐节点拒斥', () => {
    expect(isViableFallbackExit(srv('a'))).toBe(true);
    expect(isViableFallbackExit(wgSubnetOnly)).toBe(false); // subnet-only mesh
    expect(isViableFallbackExit(tsNoExit)).toBe(false); // TS 无 exitNode
    expect(isViableFallbackExit(incompleteVless)).toBe(false); // 配置不齐
  });
});

describe('fallbackExitAfterDelete', () => {
  it('删的不是当前选中节点 → 无需兜底（后端不查）', () => {
    expect(fallbackExitAfterDelete(servers, 'a', new Set(['b']), {})).toBeUndefined();
  });

  it('未选中任何节点（直连/null）→ 无需兜底', () => {
    expect(fallbackExitAfterDelete(servers, null, new Set(['a']), {})).toBeUndefined();
    expect(fallbackExitAfterDelete(servers, undefined, new Set(['a']), {})).toBeUndefined();
  });

  it('删当前选中节点 → 取剩余节点里最快的（绝不返回被删节点自身）', () => {
    const fb = fallbackExitAfterDelete(servers, 'a', new Set(['a']), { a: 10, b: 200, c: 50 });
    expect(fb).toBe('c'); // a 最快但已被删；剩余里 c(50) < b(200)
    expect(fb).not.toBe('a');
  });

  it('剩余节点无任何有效测速值 → 回退候选首个（非 undefined，否则后端落直连）', () => {
    expect(fallbackExitAfterDelete(servers, 'a', new Set(['a']), { a: 10 })).toBe('b');
  });

  it('超时(null)/未测(undefined) 不参与「最快」比较，仅靠回退兜住', () => {
    // b 超时、c 未测 → 无正值 → 回退候选首个 b。
    expect(fallbackExitAfterDelete(servers, 'a', new Set(['a']), { b: null, c: undefined })).toBe('b');
    // c 有正值、b 超时 → 选 c（超时不得因「数值小」被选中）。
    expect(fallbackExitAfterDelete(servers, 'a', new Set(['a']), { b: null, c: 80 })).toBe('c');
  });

  it('批量删除：选中节点在删除集内 → 从删除集外的剩余节点取最快', () => {
    const fb = fallbackExitAfterDelete(servers, 'a', new Set(['a', 'c']), { b: 200, c: 5 });
    expect(fb).toBe('b'); // c 更快但也在删除集内
  });

  it('批量删除：选中节点不在删除集内 → 无需兜底', () => {
    expect(fallbackExitAfterDelete(servers, 'b', new Set(['a', 'c']), {})).toBeUndefined();
  });

  it('删光了（无剩余候选）→ undefined，交后端落直连哨兵', () => {
    expect(
      fallbackExitAfterDelete(servers, 'a', new Set(['a', 'b', 'c']), { b: 10 })
    ).toBeUndefined();
  });

  // ── #291：可用性谓词拒斥 ──────────────────────────────────────────────
  it('剩余里全是 subnet-only / 配置不齐节点 → undefined（绝不静默选中裸奔候选）', () => {
    const list = [srv('a'), wgSubnetOnly, tsNoExit, incompleteVless];
    // 删选中 a，剩余 = wg(subnet-only) / ts(无 exit) / bad(不齐)，无一可承载全隧道 → 落显式直连。
    expect(fallbackExitAfterDelete(list, 'a', new Set(['a']), { wg: 5, ts: 5, bad: 5 })).toBeUndefined();
  });

  it('subnet-only 节点即便延迟最低也被跳过，选下一个可用出口', () => {
    const list = [srv('a'), wgSubnetOnly, srv('b')];
    // wg 延迟最低(1ms) 但不可承载全隧道 → 必须被谓词剔除，选 b（唯一可用剩余）。
    const fb = fallbackExitAfterDelete(list, 'a', new Set(['a']), { wg: 1, b: 90 });
    expect(fb).toBe('b');
  });

  it('组网节点承载全隧道时（WG allowInternet 默认开）可作兜底', () => {
    const wgFull = srv('wgf', {
      protocol: 'wireguard',
      uuid: undefined,
      wireguardSettings: {
        privateKey: 'priv',
        peerPublicKey: 'pub',
        localAddress: ['10.0.0.2/32'],
        // allowInternet 缺省 = 开 → meshNodeCarriesFullTunnel=true
      } as ServerConfig['wireguardSettings'],
    });
    const list = [srv('a'), wgFull];
    expect(isViableFallbackExit(wgFull)).toBe(true);
    expect(fallbackExitAfterDelete(list, 'a', new Set(['a']), { wgf: 30 })).toBe('wgf');
  });
});
