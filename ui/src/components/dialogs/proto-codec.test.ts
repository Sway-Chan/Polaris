/**
 * protoCodec 往返对称 + 淬火单测（vitest，node 环境）。
 *
 * 给 R5 真牙：逐协议 `toConfig(fromConfig(cfg), cfg)` ⊇ cfg 关键字段（对称性回归门）。
 * 另验 R2（parseNumberField 空→undefined、异常→undefined）、R3/R4（fromConfig 大小写归一）。
 */

import { describe, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ServerConfig } from '@/contracts/types';

/**
 * `Csel` 在 render 期无条件求值 `createPortal(menuNode, document.body)`，node 环境（无 jsdom）下
 * 直接抛 `Target container is not a DOM element` ⇒ 真组件渲染不了。换成一个只把收到的选项值摊平进
 * DOM 属性的替身，即可对「`FieldRenderer` 到底把哪些选项交给了下拉」下**行为断言**（而不是源码 grep）。
 * 本文件其余用例都是纯函数，不渲染任何东西，故整文件级 mock 无副作用。
 */
vi.mock('./Csel', async () => {
  const { createElement: h } = await import('react');
  return {
    Csel: (props: { options: readonly { value: string }[] }) =>
      h('div', { 'data-opts': props.options.map((o) => o.value).join('|') }),
  };
});
import { protoCodec } from './proto-codec';
import {
  PROTO_OPTIONS,
  ND_SPEC,
  allFields,
  whenTls,
  whenHttpTls,
  whenReality,
  whenWsLike,
  whenWs,
  whenGrpc,
  whenH2,
  whenMuxAvail,
  type NodeProto,
} from './node-spec';
import { parseNumberField, draftFromSpecs, toCselOptions, FieldRenderer } from './field-spec';
import type { FormValue, FormValues } from './field-spec';

const META: Pick<ServerConfig, 'id' | 'name' | 'address' | 'port'> = {
  id: 'srv-1',
  name: '香港 01',
  address: 'example.com',
  port: 443,
};

/** 每协议一个填满「关键字段」的代表性 config（值均为规范小写，往返应为恒等）。 */
const SAMPLES: Record<NodeProto, ServerConfig> = {
  vless: {
    ...META,
    protocol: 'vless',
    uuid: 'uuid-vless',
    flow: 'xtls-rprx-vision',
    network: 'ws',
    security: 'reality',
    tlsSettings: { serverName: 'sni.example', fingerprint: 'chrome', allowInsecure: true },
    realitySettings: { publicKey: 'pub-key', shortId: 'ab12' },
    wsSettings: { path: '/vl-ws', headers: { Host: 'vl.host' } },
  },
  vmess: {
    ...META,
    protocol: 'vmess',
    uuid: 'uuid-vmess',
    alterId: 0,
    vmessSecurity: 'aes-128-gcm',
    network: 'grpc',
    security: 'tls',
    tlsSettings: { serverName: 'vm.example', fingerprint: 'firefox', allowInsecure: false },
    grpcSettings: { serviceName: 'GunService' },
  },
  trojan: {
    ...META,
    protocol: 'trojan',
    password: 'trojan-pwd',
    network: 'ws',
    security: 'tls',
    tlsSettings: { serverName: 'tj.example', fingerprint: 'chrome', alpn: ['h2', 'http/1.1'] },
    wsSettings: { path: '/tj-ws', headers: { Host: 'tj.host' } },
  },
  shadowsocks: {
    ...META,
    protocol: 'shadowsocks',
    shadowsocksSettings: { method: 'aes-256-gcm', password: 'ss-pwd' },
    shadowTlsSettings: { password: 'stls-pwd', sni: 'stls.example', fingerprint: 'firefox', port: 8443 },
  },
  hysteria2: {
    ...META,
    protocol: 'hysteria2',
    password: 'hy2-pwd',
    hysteria2Settings: {
      upMbps: 100,
      downMbps: 500,
      serverPorts: '20000:30000',
      hopInterval: '30s',
      bbrProfile: 'aggressive',
      obfs: { type: 'gecko', password: 'obfs-pwd', minPacketSize: 100, maxPacketSize: 200 },
    },
    tlsSettings: { serverName: 'hy2.example', allowInsecure: true, ech: true, echConfig: 'ECHCONFIGBASE64==' },
  },
  tuic: {
    ...META,
    protocol: 'tuic',
    uuid: 'uuid-tuic',
    password: 'tuic-pwd',
    tuicSettings: { congestionControl: 'bbr', udpRelayMode: 'quic' },
    tlsSettings: { serverName: 'tuic.example', allowInsecure: true, alpn: ['h3'], ech: true, echConfig: 'TUICECH==' },
  },
  socks: {
    ...META,
    protocol: 'socks',
    username: 'user1',
    password: 'socks-pwd',
  },
  http: {
    ...META,
    protocol: 'http',
    username: 'user1',
    password: 'http-pwd',
    security: 'tls',
    tlsSettings: { serverName: 'http.example', allowInsecure: true },
  },
  anytls: {
    ...META,
    protocol: 'anytls',
    password: 'anytls-pwd',
    security: 'reality',
    tlsSettings: { serverName: 'at.example', fingerprint: 'chrome', allowInsecure: true },
    realitySettings: { publicKey: 'at-pub-key', shortId: 'cd34' },
    anyTlsSettings: { idleSessionCheckInterval: '30s', idleSessionTimeout: '60s', minIdleSession: 2 },
  },
  naive: {
    ...META,
    protocol: 'naive',
    username: 'naive-user',
    password: 'naive-pwd',
    tlsSettings: { serverName: 'nv.example' },
    naiveSettings: { useHttp3: true },
  },
  snell: {
    ...META,
    protocol: 'snell',
    password: 'snell-psk',
    snellSettings: { version: 4, obfsMode: 'http', obfsHost: 'bing.com', reuse: true, network: 'tcp' },
  },
  ssh: {
    ...META,
    protocol: 'ssh',
    sshSettings: { user: 'root', password: 'ssh-pwd', hostKey: ['ssh-ed25519 AAAA'], clientVersion: 'SSH-2.0-Polaris' },
  },
  custom: {
    ...META,
    protocol: 'custom',
    customSettings: { outbound: { type: 'shadowtls', tag: 'ignored' }, isEndpoint: true, secretKeys: ['password'] },
  },
  // Hysteria v1（2026-08-11）：obfs 是**裸口令串**、认证走 authStr —— 与 hysteria2 同名不同义，
  // 往返把这两处钉死，防止哪天有人「顺手」复用 hy2 那条腿。
  hysteria: {
    ...META,
    protocol: 'hysteria',
    hysteriaSettings: { authStr: 'hy1-auth', upMbps: 10, downMbps: 50, obfs: 'obfs-pw' },
    tlsSettings: { serverName: 'hy1.example', alpn: ['h3'], allowInsecure: true },
  },
  // Tor：**无 server/port**，故 SAMPLES 里也不给（META 带的 address/port 由 codec 忽略）。
  tor: {
    ...META,
    protocol: 'tor',
    torSettings: {
      executablePath: '/usr/bin/tor', dataDirectory: '/var/lib/tor', extraArgs: ['--quiet', '--x'],
      torrc: { ExitNodes: '{jp}', StrictNodes: '1' },
    },
  },
  // OpenConnect：server 是 host:port **单串**；flavor 决定按哪家商用 VPN 的方言握手。
  openconnect: {
    ...META,
    address: 'vpn.example.com',
    protocol: 'openconnect',
    openconnectSettings: {
      server: 'vpn.example.com:443', username: 'u', password: 'p',
      flavor: 'anyconnect', auth_group: 'grp', mtu: 1400, no_udp: true, system: true,
    },
  },
  // OpenVPN：证书类字段落盘是 PEM 逐行数组（内核要的形态），表单里是多行文本。
  'openvpn-client': {
    ...META,
    protocol: 'openvpn-client',
    openvpnClientSettings: {
      server: 'example.com', server_port: 443, username: 'u', password: 'p',
      network: 'tcp', cipher: 'AES-256-GCM', auth: 'SHA256', mtu: 1400,
      redirect_gateway: true,
      tls: { certificate: ['-----BEGIN CERTIFICATE-----', 'MIIB', '-----END CERTIFICATE-----'] },
    },
  },
};

/** 每协议断言往返后必须保留的关键字段（表单建模的字段）。 */
const KEY_ASSERTS: Record<NodeProto, (out: ServerConfig) => void> = {
  vless: (o) => {
    expect(o.uuid).toBe('uuid-vless');
    expect(o.flow).toBe('xtls-rprx-vision');
    expect(o.network).toBe('ws');
    expect(o.security).toBe('reality');
    expect(o.tlsSettings?.serverName).toBe('sni.example');
    expect(o.tlsSettings?.fingerprint).toBe('chrome');
    expect(o.tlsSettings?.allowInsecure).toBe(true);
    expect(o.realitySettings?.publicKey).toBe('pub-key');
    expect(o.realitySettings?.shortId).toBe('ab12');
    expect(o.wsSettings?.path).toBe('/vl-ws');
    expect(o.wsSettings?.headers?.Host).toBe('vl.host');
  },
  vmess: (o) => {
    expect(o.uuid).toBe('uuid-vmess');
    expect(o.alterId).toBe(0);
    expect(o.vmessSecurity).toBe('aes-128-gcm');
    expect(o.network).toBe('grpc');
    expect(o.security).toBe('tls');
    expect(o.tlsSettings?.serverName).toBe('vm.example');
    expect(o.tlsSettings?.fingerprint).toBe('firefox');
    expect(o.grpcSettings?.serviceName).toBe('GunService');
  },
  trojan: (o) => {
    expect(o.password).toBe('trojan-pwd');
    expect(o.network).toBe('ws');
    expect(o.security).toBe('tls');
    expect(o.tlsSettings?.serverName).toBe('tj.example');
    expect(o.tlsSettings?.fingerprint).toBe('chrome');
    expect(o.tlsSettings?.alpn).toEqual(['h2', 'http/1.1']);
    expect(o.wsSettings?.path).toBe('/tj-ws');
    expect(o.wsSettings?.headers?.Host).toBe('tj.host');
  },
  shadowsocks: (o) => {
    expect(o.shadowsocksSettings?.method).toBe('aes-256-gcm');
    expect(o.shadowsocksSettings?.password).toBe('ss-pwd');
    expect(o.shadowTlsSettings?.password).toBe('stls-pwd');
    expect(o.shadowTlsSettings?.sni).toBe('stls.example');
    expect(o.shadowTlsSettings?.fingerprint).toBe('firefox');
    expect(o.shadowTlsSettings?.port).toBe(8443);
  },
  hysteria2: (o) => {
    expect(o.password).toBe('hy2-pwd');
    expect(o.hysteria2Settings?.upMbps).toBe(100);
    expect(o.hysteria2Settings?.downMbps).toBe(500);
    expect(o.hysteria2Settings?.serverPorts).toBe('20000:30000');
    expect(o.hysteria2Settings?.hopInterval).toBe('30s');
    expect(o.hysteria2Settings?.bbrProfile).toBe('aggressive');
    expect(o.hysteria2Settings?.obfs?.type).toBe('gecko');
    expect(o.hysteria2Settings?.obfs?.password).toBe('obfs-pwd');
    expect(o.hysteria2Settings?.obfs?.minPacketSize).toBe(100);
    expect(o.hysteria2Settings?.obfs?.maxPacketSize).toBe(200);
    expect(o.tlsSettings?.serverName).toBe('hy2.example');
    expect(o.tlsSettings?.allowInsecure).toBe(true);
    expect(o.tlsSettings?.ech).toBe(true);
    expect(o.tlsSettings?.echConfig).toBe('ECHCONFIGBASE64==');
  },
  tuic: (o) => {
    expect(o.uuid).toBe('uuid-tuic');
    expect(o.password).toBe('tuic-pwd');
    expect(o.tuicSettings?.congestionControl).toBe('bbr');
    expect(o.tuicSettings?.udpRelayMode).toBe('quic');
    expect(o.tlsSettings?.serverName).toBe('tuic.example');
    expect(o.tlsSettings?.allowInsecure).toBe(true);
    expect(o.tlsSettings?.alpn).toEqual(['h3']);
    expect(o.tlsSettings?.ech).toBe(true);
    expect(o.tlsSettings?.echConfig).toBe('TUICECH==');
  },
  socks: (o) => {
    expect(o.username).toBe('user1');
    expect(o.password).toBe('socks-pwd');
  },
  http: (o) => {
    expect(o.username).toBe('user1');
    expect(o.password).toBe('http-pwd');
    expect(o.security).toBe('tls');
    expect(o.tlsSettings?.serverName).toBe('http.example');
    expect(o.tlsSettings?.allowInsecure).toBe(true);
  },
  anytls: (o) => {
    expect(o.password).toBe('anytls-pwd');
    expect(o.security).toBe('reality');
    expect(o.tlsSettings?.serverName).toBe('at.example');
    expect(o.tlsSettings?.fingerprint).toBe('chrome');
    expect(o.tlsSettings?.allowInsecure).toBe(true);
    expect(o.realitySettings?.publicKey).toBe('at-pub-key');
    expect(o.realitySettings?.shortId).toBe('cd34');
    expect(o.anyTlsSettings?.idleSessionCheckInterval).toBe('30s');
    expect(o.anyTlsSettings?.idleSessionTimeout).toBe('60s');
    expect(o.anyTlsSettings?.minIdleSession).toBe(2);
  },
  naive: (o) => {
    expect(o.username).toBe('naive-user');
    expect(o.password).toBe('naive-pwd');
    expect(o.tlsSettings?.serverName).toBe('nv.example');
    expect(o.naiveSettings?.useHttp3).toBe(true);
  },
  snell: (o) => {
    expect(o.password).toBe('snell-psk');
    expect(o.snellSettings?.version).toBe(4);
    expect(o.snellSettings?.obfsMode).toBe('http');
    expect(o.snellSettings?.obfsHost).toBe('bing.com');
    expect(o.snellSettings?.reuse).toBe(true);
    expect(o.snellSettings?.network).toBe('tcp');
  },
  ssh: (o) => {
    expect(o.sshSettings?.user).toBe('root');
    expect(o.sshSettings?.password).toBe('ssh-pwd');
    expect(o.sshSettings?.hostKey).toEqual(['ssh-ed25519 AAAA']);
    expect(o.sshSettings?.clientVersion).toBe('SSH-2.0-Polaris');
  },
  custom: (o) => {
    expect(o.customSettings?.outbound.type).toBe('shadowtls');
    expect(o.customSettings?.isEndpoint).toBe(true);
    expect(o.customSettings?.secretKeys).toEqual(['password']);
  },
  hysteria: (o) => {
    expect(o.hysteriaSettings?.authStr).toBe('hy1-auth');
    expect(o.hysteriaSettings?.upMbps).toBe(10);
    expect(o.hysteriaSettings?.downMbps).toBe(50);
    // 裸字符串，不是 {type,password} —— 形状错了这里就红
    expect(o.hysteriaSettings?.obfs).toBe('obfs-pw');
    expect(o.tlsSettings?.serverName).toBe('hy1.example');
    expect(o.tlsSettings?.alpn).toEqual(['h3']);
    expect(o.tlsSettings?.allowInsecure).toBe(true);
  },
  tor: (o) => {
    expect(o.torSettings?.executablePath).toBe('/usr/bin/tor');
    expect(o.torSettings?.dataDirectory).toBe('/var/lib/tor');
    expect(o.torSettings?.extraArgs).toEqual(['--quiet', '--x']);
    // torrc 是 map，表单里走 torrc 原生语法往返；键值必须逐条活着。
    expect(o.torSettings?.torrc).toEqual({ ExitNodes: '{jp}', StrictNodes: '1' });
  },
  openconnect: (o) => {
    expect(o.openconnectSettings?.server).toBe('vpn.example.com:443');
    expect(o.openconnectSettings?.username).toBe('u');
    expect(o.openconnectSettings?.flavor).toBe('anyconnect');
    expect(o.openconnectSettings?.auth_group).toBe('grp');
    expect(o.openconnectSettings?.mtu).toBe(1400);
    expect(o.openconnectSettings?.no_udp).toBe(true);
    expect(o.openconnectSettings?.system).toBe(true);
  },
  'openvpn-client': (o) => {
    expect(o.openvpnClientSettings?.username).toBe('u');
    expect(o.openvpnClientSettings?.network).toBe('tcp');
    expect(o.openvpnClientSettings?.cipher).toBe('AES-256-GCM');
    expect(o.openvpnClientSettings?.auth).toBe('SHA256');
    expect(o.openvpnClientSettings?.redirect_gateway).toBe(true);
    // PEM 逐行数组必须原样往返 —— 拼成单串或丢行都会让内核收不下
    expect(o.openvpnClientSettings?.tls?.certificate).toEqual([
      '-----BEGIN CERTIFICATE-----', 'MIIB', '-----END CERTIFICATE-----',
    ]);
  },
};

describe('protoCodec round-trip (R5)', () => {
  const protos = PROTO_OPTIONS.map(([p]) => p);

  it('覆盖全部 17 协议（不含 wireguard/tailscale，见 node-spec.ts 文件头注释）', () => {
    expect(protos).toHaveLength(17);
    expect(Object.keys(protoCodec).sort()).toEqual([...protos].sort());
  });

  for (const proto of PROTO_OPTIONS.map(([p]) => p)) {
    it(`${proto}: toConfig(fromConfig(cfg)) ⊇ 关键字段`, () => {
      const cfg = SAMPLES[proto];
      const draft = protoCodec[proto].fromConfig(cfg);
      const out = protoCodec[proto].toConfig(draft, cfg);
      // 元数据保全（R5 延伸）
      expect(out.id).toBe(cfg.id);
      expect(out.name).toBe(cfg.name);
      expect(out.address).toBe(cfg.address);
      expect(out.port).toBe(cfg.port);
      // 协议关键字段对称
      KEY_ASSERTS[proto](out);
    });
  }

  // 探针换过一次：原来用 `multiplexSettings` / `tlsSettings.ech` 当「未建模」样本，两者**都已建模**
  // （multiplex 与 TLS 高级三件套那批），照旧断言会变成在测「已建模字段的往返」，与本条要守的
  // 不变式（base 起底保全**没有控件**的字段）不再是一回事。改用 `tlsSettings.fragment` ——
  // 它至今仍在 `PORT_DEBT.TlsSettings` 里，是唯一还活着的同类样本。
  it('保全未建模的高级字段（R5：编辑不丢 detour/subscriptionId/tls.fragment 等）', () => {
    const cfg: ServerConfig = {
      ...SAMPLES.vless,
      detour: 'srv-front',
      subscriptionId: 'sub-9',
      tlsSettings: { serverName: 'sni.example', fingerprint: 'chrome', fragment: true },
    };
    const out = protoCodec.vless.toConfig(protoCodec.vless.fromConfig(cfg), cfg);
    expect(out.detour).toBe('srv-front');
    expect(out.subscriptionId).toBe('sub-9');
    expect(out.tlsSettings?.fragment).toBe(true); // 未建模的 tls 项经 mergeTls 保留
  });
});

describe('fromConfig 归一（R3/R4）', () => {
  it('R3：network/security/vmessSecurity 大写 → 小写', () => {
    const d = protoCodec.vmess.fromConfig({
      ...META,
      protocol: 'vmess',
      network: 'WS' as never,
      security: 'TLS' as never,
      vmessSecurity: 'AES-128-GCM',
    });
    expect(d.net).toBe('ws');
    expect(d.sec).toBe('tls');
    expect(d.enc).toBe('aes-128-gcm');
  });

  it('R3：tuic congestionControl/udpRelayMode 大写 → 小写', () => {
    const d = protoCodec.tuic.fromConfig({
      ...META,
      protocol: 'tuic',
      tuicSettings: { congestionControl: 'BBR' as never, udpRelayMode: 'QUIC' as never },
    });
    expect(d.cc).toBe('bbr');
    expect(d.udp).toBe('quic');
  });

  it('R4：uTLS fingerprint / vless flow 大写 → 小写', () => {
    const d = protoCodec.vless.fromConfig({
      ...META,
      protocol: 'vless',
      flow: 'XTLS-RPRX-VISION',
      tlsSettings: { fingerprint: 'Chrome' },
    });
    expect(d.fp).toBe('chrome');
    expect(d.flow).toBe('xtls-rprx-vision');
  });

  it('R3：http security==="tls" → tls 开关（isHttps 归一）', () => {
    expect(protoCodec.http.fromConfig({ ...META, protocol: 'http', security: 'TLS' as never }).tls).toBe(true);
    expect(protoCodec.http.fromConfig({ ...META, protocol: 'http', security: 'none' }).tls).toBe(false);
  });
});

describe('HIGH-1：toConfig 尊重 when 显隐 —— 隐藏字段不下发/清除（phantom tlsSettings → 明文误开 TLS）', () => {
  // #1：全新 vless（NodeDialog 新建路径 = draftFromSpecs），security 默认 'none' 但 fp 被 seed 成 O_FP 首项 'chrome'。
  // 旧实现无条件 mergeTls → tlsSettings:{fingerprint:'chrome'} → Rust tls_settings.is_some() → 对明文开 TLS → 代理死。
  it('#1 新建 vless（security=none 默认）→ 无 tlsSettings / realitySettings', () => {
    const draft = draftFromSpecs(allFields('vless'));
    expect(draft.sec).toBe('none'); // sec 选项首项
    expect(draft.fp).toBe('chrome'); // fp 被 seed 成 O_FP 首项（phantom 源）
    const out = protoCodec.vless.toConfig(draft, { ...META, protocol: 'vless' });
    expect(out.tlsSettings).toBeUndefined();
    expect(out.realitySettings).toBeUndefined();
  });

  // #2：既有 vless security='tls'（base 带 sni/fp/ech）→ 用户切回 'none' → 整块清除，TLS 关得掉。
  it('#2 vless 编辑 tls→none：tlsSettings 整块清除（含 base 未建模 ech）', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'vless',
      uuid: 'u',
      security: 'tls',
      tlsSettings: { serverName: 'sni.example', fingerprint: 'chrome', ech: true },
    };
    const draft = protoCodec.vless.fromConfig(existing);
    draft.sec = 'none'; // 用户在表单里切回明文
    const out = protoCodec.vless.toConfig(draft, existing);
    expect(out.security).toBe('none');
    expect(out.tlsSettings).toBeUndefined(); // 旧实现残留 {serverName,fingerprint,ech}
  });

  // #3：vmess / trojan（TLS 组同样 when-gated）编辑 tls→none 亦清除。
  it('#3a vmess 编辑 tls→none：tlsSettings 清除', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'vmess',
      uuid: 'u',
      security: 'tls',
      tlsSettings: { serverName: 'vm.example', allowInsecure: true },
    };
    const draft = protoCodec.vmess.fromConfig(existing);
    draft.sec = 'none';
    const out = protoCodec.vmess.toConfig(draft, existing);
    expect(out.security).toBe('none');
    expect(out.tlsSettings).toBeUndefined();
  });

  it('#3b trojan 编辑 tls→none：tlsSettings 清除', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'trojan',
      security: 'tls',
      tlsSettings: { serverName: 'tj.example' },
    };
    const draft = protoCodec.trojan.fromConfig(existing);
    draft.sec = 'none';
    const out = protoCodec.trojan.toConfig(draft, existing);
    expect(out.security).toBe('none');
    expect(out.tlsSettings).toBeUndefined();
  });

  // LOW-9（并入 HIGH-1 修复）：可见的可选字段清空 → 从 config 删除，不回落 base 陈旧值。
  it('LOW-9a vless reality 下清空 pbk → realitySettings 整块清除（不留 base 陈旧 publicKey）', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'vless',
      uuid: 'u',
      security: 'reality',
      realitySettings: { publicKey: 'old-pub', shortId: 'ab12' },
    };
    const draft = protoCodec.vless.fromConfig(existing);
    draft.pbk = ''; // 用户清空公钥
    const out = protoCodec.vless.toConfig(draft, existing);
    expect(out.realitySettings).toBeUndefined(); // 旧实现回落 base.realitySettings 留 {publicKey:'old-pub'}
  });

  it('LOW-9b vless tls 下清空 sni → 仅删 serverName，未清空的 fingerprint 保留', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'vless',
      uuid: 'u',
      security: 'tls',
      tlsSettings: { serverName: 'old.sni', fingerprint: 'chrome' },
    };
    const draft = protoCodec.vless.fromConfig(existing);
    draft.sni = '';
    const out = protoCodec.vless.toConfig(draft, existing);
    expect(out.tlsSettings?.serverName).toBeUndefined();
    expect(out.tlsSettings?.fingerprint).toBe('chrome');
  });
});

describe('MEDIUM-2：无牙归一补测（删对应 lc() 即 RED）', () => {
  it('vless network/security 大写 → 小写', () => {
    const d = protoCodec.vless.fromConfig({
      ...META,
      protocol: 'vless',
      network: 'TCP' as never,
      security: 'TLS' as never,
    });
    expect(d.net).toBe('tcp');
    expect(d.sec).toBe('tls');
  });

  it('trojan network/security 大写 → 小写', () => {
    const d = protoCodec.trojan.fromConfig({
      ...META,
      protocol: 'trojan',
      network: 'WS' as never,
      security: 'TLS' as never,
    });
    expect(d.net).toBe('ws');
    expect(d.sec).toBe('tls');
  });

  it('shadowsocks method 大写 → 小写', () => {
    const d = protoCodec.shadowsocks.fromConfig({
      ...META,
      protocol: 'shadowsocks',
      shadowsocksSettings: { method: 'AES-256-GCM', password: 'p' },
    });
    expect(d.method).toBe('aes-256-gcm');
  });

  it('hysteria2 obfs.type 大写 → 小写（补 :188 无归一）', () => {
    const d = protoCodec.hysteria2.fromConfig({
      ...META,
      protocol: 'hysteria2',
      hysteria2Settings: { obfs: { type: 'Salamander' as never } },
    });
    expect(d.obfs).toBe('salamander');
  });

  it('hysteria2 obfs 选项集覆盖契约全集（salamander + gecko）→ 存量值不渲染空下拉', () => {
    const obfsField = ND_SPEC.hysteria2.adv.find((f) => f.k === 'obfs');
    const values = obfsField && obfsField.t === 'select' ? obfsField.options.map(([v]) => v) : [];
    expect(values).toContain('salamander');
    expect(values).toContain('gecko'); // Hysteria2ObfsType = 'salamander' | 'gecko'（sing-box 1.14）
    // fromConfig 能收到的每个契约值都必须在选项里（否则下拉空白）。
    const geckoDraft = protoCodec.hysteria2.fromConfig({
      ...META,
      protocol: 'hysteria2',
      hysteria2Settings: { obfs: { type: 'gecko' } },
    });
    expect(values).toContain(geckoDraft.obfs);
  });
});

describe('FX-ech-form：hysteria2/tuic 高级字段（obfs 包长/bbr/hopInterval/ECH）回填 + 下发', () => {
  it('hysteria2 fromConfig 回填 bbr/hopInterval/gecko 包长/ech 到草稿（存量编辑不丢）', () => {
    const d = protoCodec.hysteria2.fromConfig(SAMPLES.hysteria2);
    expect(d.obfs).toBe('gecko');
    expect(d.obfsMin).toBe(100);
    expect(d.obfsMax).toBe(200);
    expect(d.bbr).toBe('aggressive');
    expect(d.hopInterval).toBe('30s');
    expect(d.ech).toBe(true);
    expect(d.echConfig).toBe('ECHCONFIGBASE64==');
  });

  it('hysteria2 toConfig 下发 tls.ech + bbrProfile + hopInterval（删任一即 RED）', () => {
    const draft = protoCodec.hysteria2.fromConfig(SAMPLES.hysteria2);
    const out = protoCodec.hysteria2.toConfig(draft, SAMPLES.hysteria2);
    expect(out.tlsSettings?.ech).toBe(true);
    expect(out.hysteria2Settings?.bbrProfile).toBe('aggressive');
    expect(out.hysteria2Settings?.hopInterval).toBe('30s');
  });

  it('hysteria2 R3：bbrProfile 大写 → 小写归一', () => {
    const d = protoCodec.hysteria2.fromConfig({
      ...META,
      protocol: 'hysteria2',
      hysteria2Settings: { bbrProfile: 'AGGRESSIVE' as never },
    });
    expect(d.bbr).toBe('aggressive');
  });

  // disable_chrome_parrot（sing-box 1.14.0-beta.7）：核心默认 false=拟态开，故「没开」必须是**键不存在**
  // 而不是 `false`——写 false 会给每份存量配置多一个语义等价的键（Rust 侧金样同理，见 builder/outbound.rs）。
  it('hysteria2 noParrot 默认关 → 不下发 disableChromeParrot 键', () => {
    const draft = protoCodec.hysteria2.fromConfig(SAMPLES.hysteria2);
    expect(draft.noParrot).toBe(false);
    const out = protoCodec.hysteria2.toConfig(draft, SAMPLES.hysteria2);
    expect(out.hysteria2Settings?.disableChromeParrot).toBeUndefined();
  });

  it('hysteria2 noParrot 开 → 下发 disableChromeParrot:true，且能回填（往返对称）', () => {
    const draft = protoCodec.hysteria2.fromConfig(SAMPLES.hysteria2);
    draft.noParrot = true;
    const out = protoCodec.hysteria2.toConfig(draft, SAMPLES.hysteria2);
    expect(out.hysteria2Settings?.disableChromeParrot).toBe(true);
    expect(protoCodec.hysteria2.fromConfig(out).noParrot).toBe(true);
  });

  it('hysteria2 obfs=salamander → 不下发 gecko 包长（min/max 清空防脏下发）', () => {
    const draft = protoCodec.hysteria2.fromConfig(SAMPLES.hysteria2);
    draft.obfs = 'salamander';
    const out = protoCodec.hysteria2.toConfig(draft, SAMPLES.hysteria2);
    expect(out.hysteria2Settings?.obfs?.type).toBe('salamander');
    expect(out.hysteria2Settings?.obfs?.minPacketSize).toBeUndefined();
    expect(out.hysteria2Settings?.obfs?.maxPacketSize).toBeUndefined();
  });

  it('hysteria2 ech 关 → tlsSettings.ech 不下发，且不误删同块的 serverName', () => {
    const base: ServerConfig = {
      ...SAMPLES.hysteria2,
      tlsSettings: { serverName: 'hy2.sni', ech: true },
    };
    const draft = protoCodec.hysteria2.fromConfig(base);
    draft.ech = false;
    const out = protoCodec.hysteria2.toConfig(draft, base);
    expect(out.tlsSettings?.ech).toBeUndefined();
    expect(out.tlsSettings?.serverName).toBe('hy2.sni'); // sni 已建模 → 经草稿往返写回
  });

  it('tuic ech 往返：ech 与 alpn 并存不互斥', () => {
    const draft = protoCodec.tuic.fromConfig(SAMPLES.tuic);
    expect(draft.ech).toBe(true);
    const out = protoCodec.tuic.toConfig(draft, SAMPLES.tuic);
    expect(out.tlsSettings?.ech).toBe(true);
    expect(out.tlsSettings?.echConfig).toBe('TUICECH==');
    expect(out.tlsSettings?.alpn).toEqual(['h3']);
  });
});

// hy2/tuic 的 TLS 恒开（后端 `builder/outbound.rs` `TLS_PROTOCOLS`），移植期漏了 sni/insecure 两个控件
// （上游 `hysteria2-form.tsx`/`tuic-form.tsx` 都有）。这一组锁住表单 → codec → tlsSettings 整条链。
describe('hysteria2/tuic TLS 字段（sni/insecure）', () => {
  for (const proto of ['hysteria2', 'tuic'] as const) {
    // 最要命的回归：给这两个字段加上 vless/vmess/trojan 用的 `whenTls` 门。这两个表单没有 sec 选择器
    // ⇒ 草稿里根本没有 `sec` 键 ⇒ 谓词恒 false ⇒ NodeDialog 的 `visible()` 把控件永久过滤掉，
    // 表现与「没移植」完全一样。这里同时锁「spec 无 when」与「加了就会消失」两面。
    it(`${proto}: sni/insecure 不得带 when 门（该表单无 sec 选择器，whenTls 恒 false 会永久隐藏）`, () => {
      const fields = allFields(proto);
      expect(fields.some((f) => f.k === 'sec')).toBe(false);
      for (const k of ['sni', 'insecure']) {
        const f = fields.find((x) => x.k === k);
        expect(f, `${proto} 缺少 ${k} 字段`).toBeDefined();
        expect(f?.when).toBeUndefined();
      }
      // 正向对照：草稿真的没有 sec ⇒ 若加了门，谓词就是 false。
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(draft.sec).toBeUndefined();
      expect(whenTls(draft)).toBe(false);
    });

    it(`${proto}: fromConfig 回填 serverName/allowInsecure（存量节点编辑不显空）`, () => {
      const d = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(d.sni).toBe(SAMPLES[proto].tlsSettings?.serverName);
      expect(d.insecure).toBe(true);
    });

    it(`${proto}: insecure 开 → 下发 allowInsecure:true 并可回填`, () => {
      const cfg = SAMPLES[proto];
      const draft = protoCodec[proto].fromConfig(cfg);
      draft.insecure = true;
      draft.sni = 'edited.example';
      const out = protoCodec[proto].toConfig(draft, cfg);
      expect(out.tlsSettings?.allowInsecure).toBe(true);
      expect(out.tlsSettings?.serverName).toBe('edited.example');
      expect(protoCodec[proto].fromConfig(out).insecure).toBe(true);
    });

    it(`${proto}: insecure 关 / sni 空 → 两键都不下发（不写 false、不写空串）`, () => {
      const cfg = SAMPLES[proto];
      const draft = protoCodec[proto].fromConfig(cfg);
      draft.insecure = false;
      draft.sni = '';
      const out = protoCodec[proto].toConfig(draft, cfg);
      expect(out.tlsSettings?.allowInsecure).toBeUndefined();
      expect(out.tlsSettings?.serverName).toBeUndefined();
      // 同块其它已建模项不受牵连（ech 仍在）。
      expect(out.tlsSettings?.ech).toBe(true);
    });
  }
});

// ShadowTLS：旧实现「开关一开就写 `{password:'', sni:''}`」，而 UI 上没有任何控件能填这两个值 ⇒
// 后端 `builder/outbounds.rs` 的 `apply_shadow_tls_postprocess` 只看 `is_some()`，据此造出 password 为
// 空串、server_name 缺席的外层 shadowtls 出站并把 SS 的 detour 指过去 = **必然连不上且没有修复入口**。
// 这一组锁住「四颗控件存在 + 门有效 + 齐备才写 + 往返不丢」整条链。
describe('shadowsocks ShadowTLS 参数组（齐备才写，缺一不下发半成品）', () => {
  const STLS_KEYS = ['stlsPwd', 'stlsSni', 'stlsFp', 'stlsPort'] as const;
  /** 不带 ShadowTLS 的 ss 节点（SAMPLES.shadowsocks 是带的）。 */
  const plainSs: ServerConfig = {
    ...META,
    protocol: 'shadowsocks',
    shadowsocksSettings: { method: 'aes-256-gcm', password: 'ss-pwd' },
  };

  it('四颗控件齐备，且 `stls` 门在真实草稿上两种取值都取得到（不是恒 false 的死门）', () => {
    const fields = allFields('shadowsocks');
    for (const k of STLS_KEYS) {
      expect(fields.find((f) => f.k === k), `ShadowTLS 缺少 ${k} 控件 —— 开关会造出用户改不了的坏节点`).toBeDefined();
      expect(fields.find((f) => f.k === k)?.when, `${k} 必须挂 stls 门`).toBeDefined();
    }
    // 正向对照（hy2/tuic 那次的教训：加了门却恒 false，表现与没移植一样）。
    const draft = protoCodec.shadowsocks.fromConfig(plainSs);
    expect(draft.stls).toBe(false);
    for (const k of STLS_KEYS) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(false);
    draft.stls = true;
    for (const k of STLS_KEYS) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(true);
    // 反面对照：ss 表单没有 `sec` 键，误用 whenTls 就是一道永远打不开的死门。
    expect(fields.some((f) => f.k === 'sec')).toBe(false);
    expect(whenTls(draft)).toBe(false);
  });

  it('开关开但 password/sni 未齐 → 整块不下发（旧实现在此写出 {password:"",sni:""} 的坏节点）', () => {
    const draft = protoCodec.shadowsocks.fromConfig(plainSs);
    draft.stls = true; // 只拨开关，什么都没填
    expect(protoCodec.shadowsocks.toConfig(draft, plainSs).shadowTlsSettings).toBeUndefined();
    draft.stlsPwd = 'only-pwd'; // 只有密码
    expect(protoCodec.shadowsocks.toConfig(draft, plainSs).shadowTlsSettings).toBeUndefined();
    draft.stlsPwd = '   '; // 纯空白不算填了
    draft.stlsSni = 'only.sni';
    expect(protoCodec.shadowsocks.toConfig(draft, plainSs).shadowTlsSettings).toBeUndefined();
  });

  it('password + sni 齐备 → 四键下发并能回填（往返对称）', () => {
    const draft = protoCodec.shadowsocks.fromConfig(plainSs);
    draft.stls = true;
    draft.stlsPwd = 'stls-pwd';
    draft.stlsSni = 'stls.example';
    draft.stlsFp = 'firefox';
    draft.stlsPort = 8443;
    const out = protoCodec.shadowsocks.toConfig(draft, plainSs);
    expect(out.shadowTlsSettings).toEqual({
      password: 'stls-pwd',
      sni: 'stls.example',
      fingerprint: 'firefox',
      port: 8443,
    });
    const back = protoCodec.shadowsocks.fromConfig(out);
    expect(back.stls).toBe(true);
    expect(back.stlsPwd).toBe('stls-pwd');
    expect(back.stlsSni).toBe('stls.example');
    expect(back.stlsFp).toBe('firefox');
    expect(back.stlsPort).toBe(8443);
  });

  it('指纹/端口留空 → 两键不下发（后端回落 chrome / 节点主端口），password+sni 照常', () => {
    const draft = protoCodec.shadowsocks.fromConfig(SAMPLES.shadowsocks);
    draft.stlsFp = '';
    draft.stlsPort = undefined;
    const out = protoCodec.shadowsocks.toConfig(draft, SAMPLES.shadowsocks);
    expect(out.shadowTlsSettings?.fingerprint).toBeUndefined();
    expect(out.shadowTlsSettings?.port).toBeUndefined();
    expect(out.shadowTlsSettings?.password).toBe('stls-pwd');
    expect(out.shadowTlsSettings?.sni).toBe('stls.example');
  });

  it('关开关 → 整块移除（不留 base 陈旧 ShadowTLS）', () => {
    const draft = protoCodec.shadowsocks.fromConfig(SAMPLES.shadowsocks);
    expect(draft.stls).toBe(true);
    draft.stls = false;
    expect(protoCodec.shadowsocks.toConfig(draft, SAMPLES.shadowsocks).shadowTlsSettings).toBeUndefined();
  });

  it('R4：ShadowTLS fingerprint 大写 → 小写归一（未归一的 "Chrome" 会让 sing-box FATAL）', () => {
    const d = protoCodec.shadowsocks.fromConfig({
      ...plainSs,
      shadowTlsSettings: { password: 'p', sni: 's', fingerprint: 'Firefox' },
    });
    expect(d.stlsFp).toBe('firefox');
  });
});

// http 的 TLS 组（与 hy2/tuic 同批移植遗漏，被漏在那次范围外）。后端一直支持：打开 TLS 后
// `security='tls'` 走 `builder/outbound.rs` 与 trojan/vless 同一段装配。
describe('http TLS 字段（sni/insecure）+ 关 TLS 时清 tlsSettings', () => {
  it('门是 whenHttpTls（读 `tls` 开关）——用 whenTls 会恒 false、控件永不显示', () => {
    const fields = allFields('http');
    expect(fields.some((f) => f.k === 'sec')).toBe(false); // 本表单没有安全层选择器
    for (const k of ['sni', 'insecure']) {
      const f = fields.find((x) => x.k === k);
      expect(f, `http 缺少 ${k} 字段`).toBeDefined();
      expect(f?.when, `${k} 必须挂 tls 开关门（明文 http 下不该显示 TLS 控件）`).toBeDefined();
    }
    // 正向对照：同一份草稿上两个谓词取值相反 —— whenTls 恒 false（没有 sec 键），whenHttpTls 跟随开关。
    const draft = protoCodec.http.fromConfig(SAMPLES.http);
    expect(draft.sec).toBeUndefined();
    expect(draft.tls).toBe(true);
    expect(whenTls(draft)).toBe(false);
    expect(whenHttpTls(draft)).toBe(true);
    for (const k of ['sni', 'insecure']) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(true);
    draft.tls = false;
    expect(whenHttpTls(draft)).toBe(false);
    for (const k of ['sni', 'insecure']) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(false);
  });

  it('fromConfig 回填 serverName/allowInsecure（存量 https 代理编辑不显空）', () => {
    const d = protoCodec.http.fromConfig(SAMPLES.http);
    expect(d.sni).toBe('http.example');
    expect(d.insecure).toBe(true);
  });

  it('tls 开：sni/insecure 有值 → 下发并可回填', () => {
    const draft = protoCodec.http.fromConfig(SAMPLES.http);
    draft.sni = 'edited.example';
    draft.insecure = true;
    const out = protoCodec.http.toConfig(draft, SAMPLES.http);
    expect(out.security).toBe('tls');
    expect(out.tlsSettings?.serverName).toBe('edited.example');
    expect(out.tlsSettings?.allowInsecure).toBe(true);
    expect(protoCodec.http.fromConfig(out).insecure).toBe(true);
  });

  it('tls 开：sni 空 / insecure 关 → 两键都不下发（不写空串、不写 false）', () => {
    const draft = protoCodec.http.fromConfig(SAMPLES.http);
    draft.sni = '';
    draft.insecure = false;
    const out = protoCodec.http.toConfig(draft, SAMPLES.http);
    expect(out.tlsSettings?.serverName).toBeUndefined();
    expect(out.tlsSettings?.allowInsecure).toBeUndefined();
  });

  // HIGH-1 同型：Rust 的 `tls_settings.is_some()` 会绕过 security='none' 开 TLS ⇒ 明文口误开 TLS 静默失联。
  it('tls 关 → tlsSettings 整块清除（含 base 未建模的 ech）', () => {
    const existing: ServerConfig = {
      ...SAMPLES.http,
      tlsSettings: { serverName: 'http.example', allowInsecure: true, ech: true },
    };
    const draft = protoCodec.http.fromConfig(existing);
    draft.tls = false;
    const out = protoCodec.http.toConfig(draft, existing);
    expect(out.security).toBe('none');
    expect(out.tlsSettings).toBeUndefined();
  });
});

// ── ①ws / grpc 传输参数 ────────────────────────────────────────────────────────
//
// 这不是「少几个字段」而是**坏功能**：传输下拉一直能选 ws/grpc/httpupgrade，选完却没有任何后续
// 输入框 ⇒ `builder/outbound.rs` 的 `generate_transport_config` 落默认 `path:"/"`
// （`ws.and_then(|w| w.path).unwrap_or("/")`），而机场节点的 ws path 绝大多数不是 `/`
// ⇒ 手工建的 ws 节点必然连不上，且 UI 上没有任何字段能修（同 ShadowTLS 空壳的「半假控件」型）。
describe('ws / grpc 传输参数（vless/vmess/trojan）', () => {
  const T_PROTOS = ['vless', 'vmess', 'trojan'] as const;
  const WS_KEYS = ['wsPath', 'wsHost'] as const;

  for (const proto of T_PROTOS) {
    it(`${proto}: 三颗控件齐备，且门在真实草稿上四种传输取值都取得到（不是恒 false 的死门）`, () => {
      const fields = allFields(proto);
      for (const k of [...WS_KEYS, 'grpcServiceName']) {
        expect(fields.find((f) => f.k === k), `${proto} 缺少 ${k} 控件 —— 选了该传输就是废节点`).toBeDefined();
        expect(fields.find((f) => f.k === k)?.when, `${k} 必须挂传输门`).toBeDefined();
      }
      // 正向对照（hy2/tuic 与 http 那两次的同款教训：加了门却恒 false，表现与没移植一样）——
      // 谓词读的 `net` 键必须真实存在于草稿里，且四档取值下的显隐互不相同。
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(draft.net, `${proto} 草稿必须有 net 键，否则传输门恒 false`).toBeDefined();
      const visibleAt = (net: string) => {
        draft.net = net;
        return [...WS_KEYS, 'grpcServiceName'].filter((k) => fields.find((f) => f.k === k)?.when?.(draft));
      };
      expect(visibleAt('ws')).toEqual(['wsPath', 'wsHost']);
      expect(visibleAt('httpupgrade')).toEqual(['wsPath', 'wsHost']); // 与 ws 同读 wsSettings
      expect(visibleAt('grpc')).toEqual(['grpcServiceName']);
      expect(visibleAt('tcp')).toEqual([]);
      expect(visibleAt('http')).toEqual([]); // http/h2 读的是另一个结构体（httpSettings），本批不建模
    });

    it(`${proto}: fromConfig 回填 path/Host/serviceName（存量节点编辑不显空）`, () => {
      const cfg: ServerConfig = {
        ...SAMPLES[proto],
        network: 'ws',
        wsSettings: { path: '/legacy', headers: { Host: 'legacy.host' } },
        grpcSettings: { serviceName: 'LegacySvc' },
      };
      const d = protoCodec[proto].fromConfig(cfg);
      expect(d.wsPath).toBe('/legacy');
      expect(d.wsHost).toBe('legacy.host');
      expect(d.grpcServiceName).toBe('LegacySvc');
    });

    it(`${proto}: ws 下发 wsSettings.path + headers.Host（大写 Host —— 后端 httpupgrade 读的就是它）`, () => {
      const base: ServerConfig = { ...SAMPLES[proto], network: 'ws', wsSettings: undefined, grpcSettings: undefined };
      const draft = protoCodec[proto].fromConfig(base);
      draft.net = 'ws';
      draft.wsPath = '/new-path';
      draft.wsHost = 'new.host';
      const out = protoCodec[proto].toConfig(draft, base);
      expect(out.wsSettings?.path).toBe('/new-path');
      expect(out.wsSettings?.headers).toEqual({ Host: 'new.host' });
      expect(out.grpcSettings).toBeUndefined(); // 非当前传输 → 整块不下发
      expect(protoCodec[proto].fromConfig(out).wsHost).toBe('new.host'); // 往返对称
    });

    it(`${proto}: httpupgrade 与 ws 写同一个 wsSettings（后端同读 ws_settings，不是各写一块）`, () => {
      const base: ServerConfig = { ...SAMPLES[proto], network: 'httpupgrade', wsSettings: undefined };
      const draft = protoCodec[proto].fromConfig(base);
      draft.net = 'httpupgrade';
      draft.wsPath = '/hu';
      draft.wsHost = 'hu.host';
      const out = protoCodec[proto].toConfig(draft, base);
      expect(out.wsSettings).toEqual({ path: '/hu', headers: { Host: 'hu.host' } });
    });

    it(`${proto}: grpc 下发 serviceName；留空 → 删键（后端 unwrap_or_default 即空串，不写空串）`, () => {
      const base: ServerConfig = { ...SAMPLES[proto], network: 'grpc' };
      const draft = protoCodec[proto].fromConfig(base);
      draft.net = 'grpc';
      draft.grpcServiceName = 'MySvc';
      expect(protoCodec[proto].toConfig(draft, base).grpcSettings?.serviceName).toBe('MySvc');
      draft.grpcServiceName = '   ';
      const cleared = protoCodec[proto].toConfig(draft, base);
      expect(cleared.grpcSettings?.serviceName).toBeUndefined();
      expect(cleared.wsSettings).toBeUndefined();
    });

    it(`${proto}: path/Host 留空 → 两键都不下发（后端回落 "/" 与「不发该 header」）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'ws',
        wsSettings: { path: '/old', headers: { Host: 'old.host' } },
      };
      const draft = protoCodec[proto].fromConfig(base);
      draft.net = 'ws';
      draft.wsPath = '';
      draft.wsHost = '';
      const out = protoCodec[proto].toConfig(draft, base);
      expect(out.wsSettings?.path).toBeUndefined();
      // 只剩 Host 一个头且被清空 ⇒ 整个 headers 键消失（不留 `{}`），wsSettings 也随之为空 → undefined。
      expect(out.wsSettings).toBeUndefined();
    });

    it(`${proto}: 切回 tcp → wsSettings/grpcSettings 整块清除（不留用户以为删掉了的旧 path）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'ws',
        wsSettings: { path: '/stale' },
        grpcSettings: { serviceName: 'StaleSvc' },
      };
      const draft = protoCodec[proto].fromConfig(base);
      draft.net = 'tcp';
      const out = protoCodec[proto].toConfig(draft, base);
      expect(out.wsSettings).toBeUndefined();
      expect(out.grpcSettings).toBeUndefined();
    });

    it(`${proto}: 只增删 Host，base 里其它请求头与未建模的早数据项原样保留`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'ws',
        wsSettings: {
          path: '/keep',
          headers: { Host: 'old.host', 'User-Agent': 'polaris/1' },
          maxEarlyData: 2560,
          earlyDataHeaderName: 'Sec-WebSocket-Protocol',
        },
      };
      const draft = protoCodec[proto].fromConfig(base);
      draft.net = 'ws';
      draft.wsHost = '';
      const out = protoCodec[proto].toConfig(draft, base);
      expect(out.wsSettings?.headers).toEqual({ 'User-Agent': 'polaris/1' }); // 只删了 Host
      expect(out.wsSettings?.maxEarlyData).toBe(2560); // 本批未建模 → 起底保全，编辑不丢
      expect(out.wsSettings?.earlyDataHeaderName).toBe('Sec-WebSocket-Protocol');
    });
  }

  it('三协议共用同一份传输控件（新增协议漏接会与 vless 不一致）', () => {
    const of = (p: NodeProto) =>
      allFields(p)
        .filter((f) => ['wsPath', 'wsHost', 'grpcServiceName'].includes(f.k))
        .map((f) => f.k);
    expect(of('vmess')).toEqual(of('vless'));
    expect(of('trojan')).toEqual(of('vless'));
  });

  it('谓词本身：whenWsLike 认 ws/httpupgrade，whenGrpc 只认 grpc', () => {
    expect(whenWsLike({ net: 'ws' })).toBe(true);
    expect(whenWsLike({ net: 'httpupgrade' })).toBe(true);
    expect(whenWsLike({ net: 'grpc' })).toBe(false);
    expect(whenWsLike({})).toBe(false); // 没有 net 键的表单（hy2/tuic/anytls）上不显示
    expect(whenGrpc({ net: 'grpc' })).toBe(true);
    expect(whenGrpc({ net: 'ws' })).toBe(false);
  });
});

// ── ② anytls 的 security 选择器 + Reality ──────────────────────────────────────
//
// 后端的 Reality 装配判据是 `security.is_reality() && reality_settings.is_some()`，**不按协议门控**
// ⇒ anytls 一直支持 reality，但表单既没有 sec 选择器也没有 pbk/sid ⇒ 建不出 anytls+reality 节点。
describe('anytls：security 选择器（tls/reality）+ Reality 公钥/Short ID', () => {
  const plainAnytls: ServerConfig = {
    ...META,
    protocol: 'anytls',
    password: 'anytls-pwd',
    tlsSettings: { serverName: 'at.example' },
  };

  it('sec 只有 tls/reality 两档 —— 给 None 档就是个拨了不生效的假控件（anytls ∈ TLS_PROTOCOLS）', () => {
    const sec = allFields('anytls').find((f) => f.k === 'sec');
    expect(sec, 'anytls 缺少安全层选择器 ⇒ 建不出 reality 节点').toBeDefined();
    expect(sec?.t).toBe('select');
    const values = sec && sec.t === 'select' ? sec.options.map(([v]) => v) : [];
    expect(values).toEqual(['tls', 'reality']);
  });

  it('sni/fp/insecure 保持无门（whenTls 在两档下恒真，加门只会多一处可漂移的判据）', () => {
    const fields = allFields('anytls');
    for (const k of ['sni', 'fp', 'insecure']) {
      expect(fields.find((f) => f.k === k)?.when, `${k} 不该有 when 门`).toBeUndefined();
    }
    // 正向对照：谓词真在这份草稿上求值 —— 两档都为 true，故「恒显」与「挂 whenTls」等价。
    const draft = protoCodec.anytls.fromConfig(plainAnytls);
    expect(draft.sec).toBe('tls');
    expect(whenTls(draft)).toBe(true);
    draft.sec = 'reality';
    expect(whenTls(draft)).toBe(true);
  });

  it('pbk/sid 挂 whenReality，且在真实草稿上两种取值都取得到', () => {
    const fields = allFields('anytls');
    for (const k of ['pbk', 'sid']) {
      const f = fields.find((x) => x.k === k);
      expect(f, `anytls 缺少 ${k} 控件`).toBeDefined();
      expect(f?.when, `${k} 必须挂 reality 门`).toBeDefined();
    }
    const draft = protoCodec.anytls.fromConfig(plainAnytls);
    expect(whenReality(draft)).toBe(false);
    for (const k of ['pbk', 'sid']) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(false);
    draft.sec = 'reality';
    expect(whenReality(draft)).toBe(true);
    for (const k of ['pbk', 'sid']) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(true);
  });

  it('fromConfig：缺省 / none / 大写变体一律折成 tls（anytls TLS 恒开，没有明文态）', () => {
    expect(protoCodec.anytls.fromConfig(plainAnytls).sec).toBe('tls');
    expect(protoCodec.anytls.fromConfig({ ...plainAnytls, security: 'none' }).sec).toBe('tls');
    expect(protoCodec.anytls.fromConfig({ ...plainAnytls, security: 'REALITY' as never }).sec).toBe('reality');
  });

  it('reality + pbk → 下发 security/realitySettings，且 tlsSettings 照常（后端仍从这里取 sni/fp/insecure）', () => {
    const draft = protoCodec.anytls.fromConfig(plainAnytls);
    draft.sec = 'reality';
    draft.pbk = 'at-pub';
    draft.sid = 'ab01';
    draft.fp = 'firefox';
    const out = protoCodec.anytls.toConfig(draft, plainAnytls);
    expect(out.security).toBe('reality');
    expect(out.realitySettings).toEqual({ publicKey: 'at-pub', shortId: 'ab01' });
    expect(out.tlsSettings?.serverName).toBe('at.example');
    expect(out.tlsSettings?.fingerprint).toBe('firefox');
    expect(protoCodec.anytls.fromConfig(out).pbk).toBe('at-pub'); // 往返对称
  });

  it('切回 tls / 清空 pbk → realitySettings 整块清除（不留 base 陈旧公钥）', () => {
    const draft = protoCodec.anytls.fromConfig(SAMPLES.anytls);
    expect(draft.pbk).toBe('at-pub-key');
    draft.sec = 'tls';
    const backToTls = protoCodec.anytls.toConfig(draft, SAMPLES.anytls);
    expect(backToTls.security).toBe('tls');
    expect(backToTls.realitySettings).toBeUndefined();
    draft.sec = 'reality';
    draft.pbk = '';
    expect(protoCodec.anytls.toConfig(draft, SAMPLES.anytls).realitySettings).toBeUndefined();
  });
});

// ── ③ vmess / trojan 的 uTLS 指纹；trojan 的 ALPN ─────────────────────────────
describe('vmess / trojan：uTLS 指纹 + trojan ALPN', () => {
  it('两协议都有 fp，且挂 whenTls（正向对照：同一草稿上 sec 切 none 即隐藏）', () => {
    for (const proto of ['vmess', 'trojan'] as const) {
      const f = allFields(proto).find((x) => x.k === 'fp');
      expect(f, `${proto} 缺少 uTLS 指纹控件`).toBeDefined();
      expect(f?.when).toBeDefined();
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(draft.sec).toBe('tls');
      expect(f?.when?.(draft)).toBe(true);
      draft.sec = 'none';
      expect(f?.when?.(draft)).toBe(false);
      expect(whenTls(draft)).toBe(false);
    }
  });

  it('fp 首档是空串而非 chrome —— 后端对这两个协议的缺省是 none（新建节点不得凭空多出 utls 块）', () => {
    for (const proto of ['vmess', 'trojan'] as const) {
      const f = allFields(proto).find((x) => x.k === 'fp');
      const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
      expect(values[0], `${proto} 的 fp 首档必须是「不启用」`).toBe('');
      expect(values).toContain('chrome');
      // 新建路径（draftFromSpecs）不得把 chrome 当默认值 seed 进草稿。
      expect(draftFromSpecs(allFields(proto)).fp).toBe('');
      const out = protoCodec[proto].toConfig(
        { ...draftFromSpecs(allFields(proto)), sec: 'tls' },
        { ...META, protocol: proto }
      );
      expect(out.tlsSettings?.fingerprint).toBeUndefined();
    }
    // 阴性对照：vless/anytls 的后端缺省就是 chrome，那两张表照旧以 chrome 起头（本条改动没波及它们）。
    for (const proto of ['vless', 'anytls'] as const) {
      const f = allFields(proto).find((x) => x.k === 'fp');
      const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
      expect(values[0]).toBe('chrome');
    }
  });

  it('fp 有值 → 下发并回填；清空 → 删键（后端回落 none 即不下发 utls）', () => {
    for (const proto of ['vmess', 'trojan'] as const) {
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      draft.fp = 'safari';
      const out = protoCodec[proto].toConfig(draft, SAMPLES[proto]);
      expect(out.tlsSettings?.fingerprint).toBe('safari');
      expect(protoCodec[proto].fromConfig(out).fp).toBe('safari');
      draft.fp = '';
      expect(protoCodec[proto].toConfig(draft, SAMPLES[proto]).tlsSettings?.fingerprint).toBeUndefined();
    }
  });

  it('R4：vmess/trojan 的 fingerprint 大写 → 小写归一（未归一的 "Chrome" 会让 sing-box FATAL）', () => {
    for (const proto of ['vmess', 'trojan'] as const) {
      const d = protoCodec[proto].fromConfig({
        ...SAMPLES[proto],
        tlsSettings: { fingerprint: 'Edge' },
      });
      expect(d.fp).toBe('edge');
    }
  });

  it('trojan alpn：挂 whenTls、有值下发数组、留空删键（后端缺省 ["http/1.1"]，写空数组会顶掉它）', () => {
    const f = allFields('trojan').find((x) => x.k === 'alpn');
    expect(f, 'trojan 缺少 ALPN 控件').toBeDefined();
    expect(f?.when).toBeDefined();
    const draft = protoCodec.trojan.fromConfig(SAMPLES.trojan);
    expect(f?.when?.(draft)).toBe(true);
    draft.sec = 'none';
    expect(f?.when?.(draft)).toBe(false);

    draft.sec = 'tls';
    expect(draft.alpn).toBe('h2,http/1.1'); // fromConfig 回填（存量节点编辑不丢）
    draft.alpn = ' h3 , h2 ';
    expect(protoCodec.trojan.toConfig(draft, SAMPLES.trojan).tlsSettings?.alpn).toEqual(['h3', 'h2']);
    draft.alpn = '';
    const cleared = protoCodec.trojan.toConfig(draft, SAMPLES.trojan);
    expect(cleared.tlsSettings?.alpn).toBeUndefined();
    expect(cleared.tlsSettings?.serverName).toBe('tj.example'); // 同块其它项不受牵连
  });
});

// ── ④⑤ 下拉档位（Rust 与内核都支持，只是表里没有） ─────────────────────────────
describe('传输 / 加密下拉档位补齐', () => {
  it('④ trojan 的传输档位与 vless/vmess 同表（补回 httpupgrade / HTTP2）', () => {
    const optsOf = (p: NodeProto) => {
      const f = allFields(p).find((x) => x.k === 'net');
      return f && f.t === 'select' ? f.options.map(([v]) => v) : [];
    };
    // 后端 `generate_transport_config` 按 `network` 单分支分派、不按协议门控，排除名单只有
    // hy2/anytls/naive ⇒ trojan 与 vless/vmess 的可用档位本就相同。
    expect(optsOf('trojan')).toEqual(optsOf('vless'));
    expect(optsOf('trojan')).toEqual(optsOf('vmess'));
    expect(optsOf('trojan')).toContain('httpupgrade');
    expect(optsOf('trojan')).toContain('http');
  });

  it('⑤ vmess 加密档位补 zero（对齐 上游的 5 档，不顺手补内核的第 6 档）', () => {
    const f = allFields('vmess').find((x) => x.k === 'enc');
    const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
    expect(values).toEqual(['auto', 'aes-128-gcm', 'chacha20-poly1305', 'none', 'zero']);
    // 往返：选 zero 能存下来也回得来（vmessSecurity 是开放 String，Rust 侧不枚举）。
    const draft = protoCodec.vmess.fromConfig(SAMPLES.vmess);
    draft.enc = 'zero';
    const out = protoCodec.vmess.toConfig(draft, SAMPLES.vmess);
    expect(out.vmessSecurity).toBe('zero');
    expect(protoCodec.vmess.fromConfig(out).enc).toBe('zero');
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// 批 B（高级 / 低频）—— 后端一行未改，全部是 Rust 本来就会下发、UI 却没有控件的项
// ══════════════════════════════════════════════════════════════════════════════

// ── ① TLS 高级三件套：engine / spoof / ech ─────────────────────────────────────
describe('TLS 高级三件套（engine / spoofSni+spoofMethod / ech）', () => {
  /** 有 `sec` 选择器的四协议（门 = whenTls）；http 单独测（门 = whenHttpTls）。 */
  const SEC_PROTOS = ['vless', 'vmess', 'trojan', 'anytls'] as const;

  it('五个协议都有 engine + spoof 控件；ech 只在四个 sec 协议上（http 无，同 上游 http-form）', () => {
    for (const proto of [...SEC_PROTOS, 'http'] as const) {
      for (const k of ['engine', 'spoofMethod', 'spoofSni']) {
        expect(allFields(proto).find((f) => f.k === k), `${proto} 缺少 ${k} 控件`).toBeDefined();
      }
    }
    for (const proto of SEC_PROTOS) {
      expect(allFields(proto).find((f) => f.k === 'ech'), `${proto} 缺少 ECH 开关`).toBeDefined();
      expect(allFields(proto).find((f) => f.k === 'echConfig')).toBeDefined();
    }
    // http 侧是**有意**不给 ECH（两边都没有 ⇒ 不算移植遗漏），不是漏接。
    expect(allFields('http').some((f) => f.k === 'ech')).toBe(false);
  });

  it('engine 首档是空串 —— 后端只认 windows/apple 且要平台匹配，`go` 写进磁盘也永不下发', () => {
    for (const proto of [...SEC_PROTOS, 'http'] as const) {
      const f = allFields(proto).find((x) => x.k === 'engine');
      const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
      expect(values[0], `${proto} 的 engine 首档必须是「不下发」`).toBe('');
      expect(values).toEqual(['', 'windows', 'apple']);
      // 默认 seed 不得凭空写进 config（fp 那次 chrome 的同型陷阱）。
      expect(draftFromSpecs(allFields(proto)).engine).toBe('');
    }
  });

  it('spoofMethod 首档是空串，且取值集 = 后端 TLS_SPOOF_METHODS 三档（不是内核 schema 的五档）', () => {
    for (const proto of [...SEC_PROTOS, 'http'] as const) {
      const f = allFields(proto).find((x) => x.k === 'spoofMethod');
      const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
      // 随包核 beta.7 的 schema 里 spoof_method enum 还有 wrong-sequence / wrong-checksum，
      // 但 `validate_tls_spoof_default` 只放行这三个 ⇒ 多给就是选了必然不下发的假档位。
      expect(values).toEqual(['', 'wrong-ack', 'wrong-md5', 'wrong-timestamp']);
      expect(draftFromSpecs(allFields(proto)).spoofMethod).toBe('');
    }
  });

  it('门（四个 sec 协议）：三件套挂 whenTls，spoofSni 再叠「方法非空」，echConfig 再叠「ech 开」', () => {
    for (const proto of SEC_PROTOS) {
      const fields = allFields(proto);
      const at = (k: string) => fields.find((f) => f.k === k);
      for (const k of ['engine', 'spoofMethod', 'ech', 'spoofSni', 'echConfig']) {
        expect(at(k)?.when, `${proto} 的 ${k} 必须挂门`).toBeDefined();
      }
      // 正向对照：谓词读的键真的在草稿里，且逐档取值不同（批 A 的 net→network 死门形态）。
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(draft.sec, `${proto} 草稿必须有 sec 键，否则 whenTls 恒 false`).toBeDefined();
      const visible = () =>
        ['engine', 'spoofMethod', 'spoofSni', 'ech', 'echConfig'].filter((k) => at(k)?.when?.(draft));

      draft.sec = 'tls';
      draft.spoofMethod = '';
      draft.ech = false;
      expect(visible()).toEqual(['engine', 'spoofMethod', 'ech']);
      draft.spoofMethod = 'wrong-ack';
      expect(visible()).toEqual(['engine', 'spoofMethod', 'spoofSni', 'ech']);
      draft.ech = true;
      expect(visible()).toEqual(['engine', 'spoofMethod', 'spoofSni', 'ech', 'echConfig']);

      if (proto === 'anytls') {
        // anytls 只有 tls/reality 两档 ⇒ whenTls 恒真，本组门在此**恒开**。这是共用同一份
        // F_TLS_ADV 的代价，必须有正向对照证明它确实恒开（否则就是一组永不渲染的控件）。
        draft.sec = 'reality';
        expect(whenTls(draft)).toBe(true);
        // 唯一的例外是 engine —— reality 下后端会把它丢掉，见下一条用例。
        expect(visible()).toEqual(['spoofMethod', 'spoofSni', 'ech', 'echConfig']);
      } else {
        draft.sec = 'none';
        expect(whenTls(draft)).toBe(false);
        expect(visible()).toEqual([]);
      }
    }
  });

  // 🔴 后端 Reality 段把整块 TLS 换掉、新块 `engine: None` ⇒ reality 下的引擎选择必然被丢弃；
  // 而 spoof/ech 是在替换之后补上的，照常生效。Rust 侧同一事实的断言见
  // `builder/outbound.rs::reality_branch_drops_tls_engine_but_keeps_spoof_and_ech`。
  it('engine 在 reality 下隐藏（后端会丢弃），spoof/ech 不受影响', () => {
    for (const proto of ['vless', 'anytls'] as const) {
      const fields = allFields(proto);
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      draft.sec = 'tls';
      expect(fields.find((f) => f.k === 'engine')?.when?.(draft)).toBe(true);
      draft.sec = 'reality';
      expect(whenTls(draft)).toBe(true); // 正向对照：不是被一级门关掉的
      expect(fields.find((f) => f.k === 'engine')?.when?.(draft)).toBe(false);
      expect(fields.find((f) => f.k === 'spoofMethod')?.when?.(draft)).toBe(true);
      expect(fields.find((f) => f.k === 'ech')?.when?.(draft)).toBe(true);
    }
    // 阴性对照：没有 reality 档的三个协议不受这条限制（其草稿里 sec 要么是 tls/none、要么没有）。
    for (const proto of ['vmess', 'trojan', 'http'] as const) {
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(whenReality(draft)).toBe(false);
      expect(allFields(proto).find((f) => f.k === 'engine')?.when?.(draft)).toBe(true);
    }
  });

  // 隐藏 ≠ 丢值：控件在 reality 下不显示，但存量 engine 必须原样活着 —— 用户 tls⇄reality 来回切
  // 不该静默丢掉他填过的引擎，且 builder 哪天补上 engine 透传，存量值即自动生效。
  // （保全靠的是 fromConfig→toConfig 的同源往返，codec 里没有 reality 特判，见 tlsAdvPatch 注释。）
  it('engine 在 reality 下保全（隐藏但不丢值，来回切也在）', () => {
    const base: ServerConfig = {
      ...SAMPLES.vless,
      security: 'tls',
      tlsSettings: { serverName: 's.com', engine: 'windows' },
    };
    const draft = protoCodec.vless.fromConfig(base);
    expect(draft.engine).toBe('windows');
    draft.sec = 'reality';
    draft.pbk = 'pk';
    const reality = protoCodec.vless.toConfig(draft, base);
    expect(reality.tlsSettings?.engine).toBe('windows'); // 保全
    draft.sec = 'tls';
    expect(protoCodec.vless.toConfig(draft, reality).tlsSettings?.engine).toBe('windows'); // 切回仍在
  });

  it('门（http）：走 whenHttpTls 读 `tls` 开关 —— 用 whenTls 会恒 false、控件永不显示', () => {
    const fields = allFields('http');
    const at = (k: string) => fields.find((f) => f.k === k);
    const draft = protoCodec.http.fromConfig(SAMPLES.http);
    expect(draft.sec).toBeUndefined(); // 前提：本表单没有 sec 键
    expect(whenTls(draft)).toBe(false);
    expect(draft.tls).toBe(true);
    expect(at('engine')?.when?.(draft)).toBe(true);
    expect(at('spoofMethod')?.when?.(draft)).toBe(true);
    expect(at('spoofSni')?.when?.(draft)).toBe(false); // 方法为空
    draft.spoofMethod = 'wrong-md5';
    expect(at('spoofSni')?.when?.(draft)).toBe(true);
    draft.tls = false; // 关 TLS ⇒ 整组消失
    for (const k of ['engine', 'spoofMethod', 'spoofSni']) expect(at(k)?.when?.(draft)).toBe(false);
  });

  it('engine：有值下发、往返对称；清空删键（不写空串）', () => {
    for (const proto of [...SEC_PROTOS, 'http'] as const) {
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      if (draft.sec === 'reality') draft.sec = 'tls'; // reality 下 engine 不参与 patch，见专门用例
      draft.engine = 'windows';
      const out = protoCodec[proto].toConfig(draft, SAMPLES[proto]);
      expect(out.tlsSettings?.engine, `${proto} engine 未下发`).toBe('windows');
      expect(protoCodec[proto].fromConfig(out).engine).toBe('windows');
      draft.engine = '';
      expect(protoCodec[proto].toConfig(draft, SAMPLES[proto]).tlsSettings?.engine).toBeUndefined();
    }
  });

  it('R3：engine / spoofMethod 大写变体归一（后端是精确匹配，"Windows" 会永不生效）', () => {
    const d = protoCodec.vless.fromConfig({
      ...SAMPLES.vless,
      tlsSettings: { engine: 'Windows' as never, spoofMethod: 'WRONG-ACK' as never, spoofSni: 'decoy.com' },
    });
    expect(d.engine).toBe('windows');
    expect(d.spoofMethod).toBe('wrong-ack');
  });

  it('spoof **齐备才写**：只选方法 / 只填 SNI 都整对不下发（磁盘上不留永不生效的死键）', () => {
    for (const proto of [...SEC_PROTOS, 'http'] as const) {
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      const outOf = () => protoCodec[proto].toConfig(draft, SAMPLES[proto]).tlsSettings;

      draft.spoofMethod = 'wrong-ack';
      draft.spoofSni = '';
      expect(outOf()?.spoofMethod, `${proto}: 只有方法时不得写 spoofMethod`).toBeUndefined();
      expect(outOf()?.spoofSni).toBeUndefined();

      draft.spoofMethod = '';
      draft.spoofSni = 'decoy.example';
      expect(outOf()?.spoofMethod).toBeUndefined();
      expect(outOf()?.spoofSni, `${proto}: 只有 SNI 时不得写 spoofSni`).toBeUndefined();

      draft.spoofMethod = 'wrong-timestamp';
      draft.spoofSni = '  decoy.example  ';
      expect(outOf()?.spoofMethod).toBe('wrong-timestamp');
      expect(outOf()?.spoofSni).toBe('decoy.example'); // trim
      const back = protoCodec[proto].fromConfig(protoCodec[proto].toConfig(draft, SAMPLES[proto]));
      expect(back.spoofMethod).toBe('wrong-timestamp');
      expect(back.spoofSni).toBe('decoy.example');
    }
  });

  it('ech：开 → 下发 ech+echConfig；关 → 两键都删且不误伤同块其它项', () => {
    for (const proto of SEC_PROTOS) {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        tlsSettings: { ...SAMPLES[proto].tlsSettings, ech: true, echConfig: 'ECHPEM==' },
      };
      const draft = protoCodec[proto].fromConfig(base);
      expect(draft.ech, `${proto} 未回填 ech`).toBe(true);
      expect(draft.echConfig).toBe('ECHPEM==');
      expect(protoCodec[proto].toConfig(draft, base).tlsSettings?.ech).toBe(true);

      draft.ech = false;
      const off = protoCodec[proto].toConfig(draft, base).tlsSettings;
      expect(off?.ech).toBeUndefined();
      expect(off?.echConfig).toBeUndefined();
      expect(off?.serverName).toBe(SAMPLES[proto].tlsSettings?.serverName); // 同块其它项不受牵连
    }
  });

  // http 没有 ECH 控件 ⇒ 该键属「未建模」，必须走 base 起底保全，**不得**被一个不存在的控件顺手清空。
  it('http：存量节点的 ech 经编辑保全（未建模 ≠ 该删）', () => {
    const base: ServerConfig = {
      ...SAMPLES.http,
      tlsSettings: { serverName: 'http.example', ech: true, echConfig: 'HTTPECH==' },
    };
    const out = protoCodec.http.toConfig(protoCodec.http.fromConfig(base), base);
    expect(out.tlsSettings?.ech).toBe(true);
    expect(out.tlsSettings?.echConfig).toBe('HTTPECH==');
  });

  // HIGH-1：sec='none' 时整个 tlsSettings 清除，三件套一并消失（不留 phantom 让 Rust 误开 TLS）。
  it('HIGH-1 延伸：vless 切回 sec=none → engine/spoof/ech 随整块清除', () => {
    const base: ServerConfig = {
      ...SAMPLES.vless,
      security: 'tls',
      tlsSettings: { serverName: 's', engine: 'apple', spoofMethod: 'wrong-md5', spoofSni: 'd.com', ech: true },
    };
    const draft = protoCodec.vless.fromConfig(base);
    draft.sec = 'none';
    expect(protoCodec.vless.toConfig(draft, base).tlsSettings).toBeUndefined();
  });
});

// ── ② Multiplex ×5 ────────────────────────────────────────────────────────────
describe('Multiplex（vless / vmess / trojan / shadowsocks；vision flow 下禁用）', () => {
  const MUX_PROTOS = ['vless', 'vmess', 'trojan', 'shadowsocks'] as const;
  const MUX_KEYS = ['mux', 'muxProto', 'muxMax', 'muxMin', 'muxPad'] as const;
  /** vless 的 SAMPLES 带 vision flow（mux 在那下面必须消失），故另备一份普通 flow 的。 */
  const plainVless: ServerConfig = { ...SAMPLES.vless, flow: '' };
  const sampleOf = (p: (typeof MUX_PROTOS)[number]): ServerConfig =>
    p === 'vless' ? plainVless : SAMPLES[p];

  it('协议面 = 后端那句 matches!（四个有、其余没有）', () => {
    for (const proto of MUX_PROTOS) {
      for (const k of MUX_KEYS) {
        expect(allFields(proto).find((f) => f.k === k), `${proto} 缺少 ${k} 控件`).toBeDefined();
      }
    }
    // 阴性对照：后端 `matches!` 不含这些协议 ⇒ 给控件就是假控件。
    for (const proto of ['hysteria2', 'tuic', 'anytls', 'http', 'socks', 'naive', 'snell', 'ssh'] as const) {
      expect(allFields(proto).some((f) => f.k === 'mux'), `${proto} 不该有 multiplex`).toBe(false);
    }
  });

  it('muxProto 首档 h2mux 是安全 seed —— 开关默认关 ⇒ 新建节点不下发 multiplexSettings', () => {
    for (const proto of MUX_PROTOS) {
      const fresh = draftFromSpecs(allFields(proto));
      expect(fresh.mux, `${proto} 的 mux 开关默认必须是关`).toBe(false);
      expect(fresh.muxProto).toBe('h2mux'); // 被 seed 了，但下一行证明它漏不出去
      const out = protoCodec[proto].toConfig(fresh, { ...META, protocol: proto });
      expect(out.multiplexSettings, `${proto} 新建节点凭空多出 multiplexSettings`).toBeUndefined();
    }
    const f = allFields('vless').find((x) => x.k === 'muxProto');
    const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
    expect(values).toEqual(['h2mux', 'smux', 'yamux']); // = 内核 schema 的 protocol enum
  });

  it('门在真实草稿上逐档取值都取得到（开关关 → 只剩开关本身可见）', () => {
    for (const proto of MUX_PROTOS) {
      const fields = allFields(proto);
      const draft = protoCodec[proto].fromConfig(sampleOf(proto));
      const visible = () => MUX_KEYS.filter((k) => {
        const w = fields.find((f) => f.k === k)?.when;
        return w === undefined || w(draft);
      });
      draft.mux = false;
      expect(visible()).toEqual(['mux']);
      draft.mux = true;
      expect(visible()).toEqual([...MUX_KEYS]);
    }
  });

  it('vision flow：整组隐藏（后端同样跳过 multiplex，留着就是假控件）', () => {
    const fields = allFields('vless');
    const draft = protoCodec.vless.fromConfig(plainVless);
    draft.mux = true;
    expect(whenMuxAvail(draft)).toBe(true);
    expect(fields.find((f) => f.k === 'mux')?.when?.(draft)).toBe(true);
    // 后端判据是 `flow.to_ascii_lowercase().contains("vision")` —— 大小写与子串都要跟上。
    for (const flow of ['xtls-rprx-vision', 'XTLS-RPRX-VISION', 'xtls-rprx-vision-udp443']) {
      draft.flow = flow;
      expect(whenMuxAvail(draft), `flow=${flow} 必须判为不可用`).toBe(false);
      for (const k of MUX_KEYS) expect(fields.find((f) => f.k === k)?.when?.(draft)).toBe(false);
    }
    // 正向对照（反方向的死门）：没有 `flow` 键的三个表单上必须判为**可用**，
    // 否则 vmess/trojan/ss 的 multiplex 会被一个恒 false 的谓词永久隐藏。
    for (const proto of ['vmess', 'trojan', 'shadowsocks'] as const) {
      const d = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(d.flow, `${proto} 草稿不该有 flow 键`).toBeUndefined();
      expect(whenMuxAvail(d)).toBe(true);
    }
  });

  it('开 → 下发五键并往返对称；关 → 整块清除（不留 base 陈旧 multiplex）', () => {
    for (const proto of MUX_PROTOS) {
      const base = sampleOf(proto);
      const draft = protoCodec[proto].fromConfig(base);
      draft.mux = true;
      draft.muxProto = 'yamux';
      draft.muxMax = 4;
      draft.muxMin = 2;
      draft.muxPad = true;
      const out = protoCodec[proto].toConfig(draft, base);
      expect(out.multiplexSettings).toEqual({
        enabled: true,
        protocol: 'yamux',
        maxConnections: 4,
        minStreams: 2,
        padding: true,
      });
      const back = protoCodec[proto].fromConfig(out);
      expect(back.mux).toBe(true);
      expect(back.muxProto).toBe('yamux');
      expect(back.muxMax).toBe(4);
      expect(back.muxMin).toBe(2);
      expect(back.muxPad).toBe(true);

      draft.mux = false;
      expect(protoCodec[proto].toConfig(draft, out).multiplexSettings).toBeUndefined();
    }
  });

  it('数值留空 / padding 关 → 三键都不下发（不写 0、不写 false）', () => {
    const draft = protoCodec.vless.fromConfig(plainVless);
    draft.mux = true;
    draft.muxMax = undefined;
    draft.muxMin = undefined;
    draft.muxPad = false;
    const mux = protoCodec.vless.toConfig(draft, plainVless).multiplexSettings;
    expect(mux).toEqual({ enabled: true, protocol: 'h2mux' });
  });

  it('vless 选了 vision flow → multiplexSettings 整块清除（照 上游的 skipVisionFlow）', () => {
    const base: ServerConfig = {
      ...plainVless,
      multiplexSettings: { enabled: true, protocol: 'h2mux', maxConnections: 4 },
    };
    const draft = protoCodec.vless.fromConfig(base);
    expect(draft.mux).toBe(true);
    expect(protoCodec.vless.toConfig(draft, base).multiplexSettings?.enabled).toBe(true);
    draft.flow = 'xtls-rprx-vision';
    expect(protoCodec.vless.toConfig(draft, base).multiplexSettings).toBeUndefined();
  });

  it('R3：multiplex protocol 大写 → 小写归一（内核 enum 只认小写）', () => {
    const d = protoCodec.trojan.fromConfig({
      ...SAMPLES.trojan,
      multiplexSettings: { enabled: true, protocol: 'YAMUX' as never },
    });
    expect(d.muxProto).toBe('yamux');
  });
});

// ── ③ tuic zeroRttHandshake / heartbeat ───────────────────────────────────────
describe('tuic：0-RTT 握手 + 心跳间隔', () => {
  it('两颗控件齐备且无 when 门（本表单没有 sec 键，加 whenTls 会恒 false）', () => {
    const fields = allFields('tuic');
    for (const k of ['zeroRtt', 'heartbeat']) {
      const f = fields.find((x) => x.k === k);
      expect(f, `tuic 缺少 ${k} 控件`).toBeDefined();
      expect(f?.when).toBeUndefined();
    }
    const draft = protoCodec.tuic.fromConfig(SAMPLES.tuic);
    expect(draft.sec).toBeUndefined();
    expect(whenTls(draft)).toBe(false); // 正向对照：误用 whenTls 就是死门
  });

  it('开 → 下发 zeroRttHandshake:true 并回填；关 → 整键不下发（内核默认 false）', () => {
    const draft = protoCodec.tuic.fromConfig(SAMPLES.tuic);
    expect(draft.zeroRtt).toBe(false);
    expect(protoCodec.tuic.toConfig(draft, SAMPLES.tuic).tuicSettings?.zeroRttHandshake).toBeUndefined();
    draft.zeroRtt = true;
    const out = protoCodec.tuic.toConfig(draft, SAMPLES.tuic);
    expect(out.tuicSettings?.zeroRttHandshake).toBe(true);
    expect(protoCodec.tuic.fromConfig(out).zeroRtt).toBe(true);
  });

  it('heartbeat：有值下发并回填；留空删键（后端 normalize_duration 兜底单位，不在前端补）', () => {
    const draft = protoCodec.tuic.fromConfig(SAMPLES.tuic);
    expect(draft.heartbeat).toBe('');
    draft.heartbeat = ' 10s ';
    expect(protoCodec.tuic.toConfig(draft, SAMPLES.tuic).tuicSettings?.heartbeat).toBe('10s');
    draft.heartbeat = '3000'; // 裸数字原样存，后端补 ms
    expect(protoCodec.tuic.toConfig(draft, SAMPLES.tuic).tuicSettings?.heartbeat).toBe('3000');
    draft.heartbeat = '';
    expect(protoCodec.tuic.toConfig(draft, SAMPLES.tuic).tuicSettings?.heartbeat).toBeUndefined();
  });
});

// ── ④ ssh 算法协商四项 ────────────────────────────────────────────────────────
describe('ssh：hostKeyAlgorithms / cipher / mac / kexAlgorithm（逗号分隔，同 hostKey）', () => {
  const SSH_LISTS = ['hostKey', 'hostKeyAlgorithms', 'cipher', 'mac', 'kexAlgorithm'] as const;

  it('五个列表控件齐备且无 when 门（ssh 表单没有条件字段）', () => {
    for (const k of SSH_LISTS) {
      const f = allFields('ssh').find((x) => x.k === k);
      expect(f, `ssh 缺少 ${k} 控件`).toBeDefined();
      expect(f?.when).toBeUndefined();
    }
  });

  it('回填 + 下发：逗号分隔 ⇄ 数组，往返对称', () => {
    const cfg: ServerConfig = {
      ...SAMPLES.ssh,
      sshSettings: {
        ...SAMPLES.ssh.sshSettings,
        hostKeyAlgorithms: ['ssh-ed25519', 'rsa-sha2-256'],
        cipher: ['aes128-ctr'],
        mac: ['hmac-sha2-256', 'hmac-sha1'],
        kexAlgorithm: ['curve25519-sha256'],
      },
    };
    const d = protoCodec.ssh.fromConfig(cfg);
    expect(d.hostKeyAlgorithms).toBe('ssh-ed25519,rsa-sha2-256');
    expect(d.cipher).toBe('aes128-ctr');
    expect(d.mac).toBe('hmac-sha2-256,hmac-sha1');
    expect(d.kexAlgorithm).toBe('curve25519-sha256');
    const out = protoCodec.ssh.toConfig(d, cfg);
    expect(out.sshSettings?.hostKeyAlgorithms).toEqual(['ssh-ed25519', 'rsa-sha2-256']);
    expect(out.sshSettings?.cipher).toEqual(['aes128-ctr']);
    expect(out.sshSettings?.mac).toEqual(['hmac-sha2-256', 'hmac-sha1']);
    expect(out.sshSettings?.kexAlgorithm).toEqual(['curve25519-sha256']);
    expect(out.sshSettings?.hostKey).toEqual(['ssh-ed25519 AAAA']); // 既有项不受牵连
  });

  it('留空 / 纯空白 / 纯逗号 → 删键（**不写空数组**：那等于「一个算法都不接受」）', () => {
    const cfg: ServerConfig = {
      ...SAMPLES.ssh,
      sshSettings: { ...SAMPLES.ssh.sshSettings, cipher: ['aes128-ctr'], mac: ['hmac-sha1'] },
    };
    const d = protoCodec.ssh.fromConfig(cfg);
    d.cipher = '   ';
    d.mac = ' , , ';
    d.kexAlgorithm = '';
    const out = protoCodec.ssh.toConfig(d, cfg);
    expect(out.sshSettings?.cipher).toBeUndefined();
    expect(out.sshSettings?.mac).toBeUndefined();
    expect(out.sshSettings?.kexAlgorithm).toBeUndefined();
  });

  it('分隔与去空白：多空格/尾逗号不产生空项', () => {
    const d = protoCodec.ssh.fromConfig(SAMPLES.ssh);
    d.cipher = ' aes128-ctr , aes256-gcm@openssh.com ,';
    expect(protoCodec.ssh.toConfig(d, SAMPLES.ssh).sshSettings?.cipher).toEqual([
      'aes128-ctr',
      'aes256-gcm@openssh.com',
    ]);
  });
});

// ── ⑤⑥ shadowsocks：SIP003 插件 + 加密方式放开到内核全集 ────────────────────────
describe('shadowsocks：plugin / pluginOptions + method 内核全集', () => {
  const plainSs: ServerConfig = {
    ...META,
    protocol: 'shadowsocks',
    shadowsocksSettings: { method: 'aes-256-gcm', password: 'ss-pwd' },
  };

  it('两颗插件控件齐备且无 when 门', () => {
    for (const k of ['plugin', 'pluginOpts']) {
      const f = allFields('shadowsocks').find((x) => x.k === k);
      expect(f, `shadowsocks 缺少 ${k} 控件`).toBeDefined();
      expect(f?.when).toBeUndefined();
    }
  });

  it('plugin/pluginOptions：回填 + 下发 + 往返；留空删键（不写空串）', () => {
    const cfg: ServerConfig = {
      ...plainSs,
      shadowsocksSettings: {
        method: 'aes-256-gcm',
        password: 'ss-pwd',
        plugin: 'obfs-local',
        pluginOptions: 'obfs=http;obfs-host=bing.com',
      },
    };
    const d = protoCodec.shadowsocks.fromConfig(cfg);
    expect(d.plugin).toBe('obfs-local');
    expect(d.pluginOpts).toBe('obfs=http;obfs-host=bing.com');
    const out = protoCodec.shadowsocks.toConfig(d, cfg);
    expect(out.shadowsocksSettings?.plugin).toBe('obfs-local');
    expect(out.shadowsocksSettings?.pluginOptions).toBe('obfs=http;obfs-host=bing.com');
    expect(protoCodec.shadowsocks.fromConfig(out).plugin).toBe('obfs-local');

    d.plugin = '';
    d.pluginOpts = '   ';
    const cleared = protoCodec.shadowsocks.toConfig(d, cfg);
    expect(cleared.shadowsocksSettings?.plugin).toBeUndefined();
    expect(cleared.shadowsocksSettings?.pluginOptions).toBeUndefined();
    expect(cleared.shadowsocksSettings?.method).toBe('aes-256-gcm'); // 同块不受牵连
  });

  it('T5：method 取值集 = 随包核 beta.7 schema 的 18 档，首项仍是既有默认 seed', () => {
    const f = allFields('shadowsocks').find((x) => x.k === 'method');
    const values = f && f.t === 'select' ? f.options.map(([v]) => v) : [];
    // 真值源：`resources/linux/sing-box schema` → $defs/Outbound/oneOf[type=shadowsocks].method.enum。
    expect([...values].sort()).toEqual(
      [
        'none',
        'aes-128-gcm',
        'aes-192-gcm',
        'aes-256-gcm',
        'chacha20-ietf-poly1305',
        'xchacha20-ietf-poly1305',
        '2022-blake3-aes-128-gcm',
        '2022-blake3-aes-256-gcm',
        '2022-blake3-chacha20-poly1305',
        'aes-128-ctr',
        'aes-192-ctr',
        'aes-256-ctr',
        'aes-128-cfb',
        'aes-192-cfb',
        'aes-256-cfb',
        'rc4-md5',
        'chacha20-ietf',
        'xchacha20',
      ].sort()
    );
    // 首项 = draftFromSpecs 的默认 seed，换掉会静默改变新建 ss 节点的默认加密方式。
    expect(values[0]).toBe('2022-blake3-aes-128-gcm');
    expect(draftFromSpecs(allFields('shadowsocks')).method).toBe('2022-blake3-aes-128-gcm');
  });

  it('T5：此前选不到的档位现在选得到并能往返（aes-192-gcm 是那条真机反馈的原型）', () => {
    const d = protoCodec.shadowsocks.fromConfig(plainSs);
    d.method = 'aes-192-gcm';
    const out = protoCodec.shadowsocks.toConfig(d, plainSs);
    expect(out.shadowsocksSettings?.method).toBe('aes-192-gcm');
    expect(protoCodec.shadowsocks.fromConfig(out).method).toBe('aes-192-gcm');
  });
});

// ── ⑦ ws 早数据（maxEarlyData / earlyDataHeaderName） ─────────────────────────
//
// grpc 的 `multiMode` **有意不补**（不是漏做）：`generate_transport_config` 的 grpc 腿不下发它、
// `singbox::Transport` 里根本没有这个字段、随包核 schema 的 grpc 传输也没有（`additionalProperties:false`）
// ⇒ 给它控件就是一个拨了永远不生效的假开关。理由与证据链见 `protocol-settings-coverage.test.ts`
// 的 `PORT_DEBT.GrpcSettings` 注释。
describe('ws 早数据（只在 net=ws；?ed= 在路径里时后端以路径为准）', () => {
  const T_PROTOS = ['vless', 'vmess', 'trojan'] as const;
  const ED_KEYS = ['wsMaxEarlyData', 'wsEdHeader'] as const;

  for (const proto of T_PROTOS) {
    it(`${proto}: 两颗控件挂 whenWs —— httpupgrade 下必须隐藏（后端那条腿不读这两键）`, () => {
      const fields = allFields(proto);
      for (const k of ED_KEYS) {
        expect(fields.find((f) => f.k === k), `${proto} 缺少 ${k} 控件`).toBeDefined();
        expect(fields.find((f) => f.k === k)?.when).toBeDefined();
      }
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      expect(draft.net).toBeDefined(); // 正向对照：谓词读的键真的存在
      const visibleAt = (net: string) => {
        draft.net = net;
        return ED_KEYS.filter((k) => fields.find((f) => f.k === k)?.when?.(draft));
      };
      expect(visibleAt('ws')).toEqual([...ED_KEYS]);
      expect(visibleAt('httpupgrade')).toEqual([]); // 与 path/Host 的 whenWsLike 分道扬镳之处
      expect(visibleAt('grpc')).toEqual([]);
      expect(visibleAt('tcp')).toEqual([]);
    });

    it(`${proto}: ws 下回填 + 下发 + 往返；清空删键（不写 0 / 空串）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'ws',
        wsSettings: { path: '/p', maxEarlyData: 2560, earlyDataHeaderName: 'X-Ed' },
      };
      const d = protoCodec[proto].fromConfig(base);
      expect(d.wsMaxEarlyData).toBe(2560);
      expect(d.wsEdHeader).toBe('X-Ed');
      d.net = 'ws';
      d.wsMaxEarlyData = 1024;
      d.wsEdHeader = 'Sec-WebSocket-Protocol';
      const out = protoCodec[proto].toConfig(d, base);
      expect(out.wsSettings?.maxEarlyData).toBe(1024);
      expect(out.wsSettings?.earlyDataHeaderName).toBe('Sec-WebSocket-Protocol');
      expect(protoCodec[proto].fromConfig(out).wsMaxEarlyData).toBe(1024);

      d.wsMaxEarlyData = undefined;
      d.wsEdHeader = '';
      const cleared = protoCodec[proto].toConfig(d, base);
      expect(cleared.wsSettings?.maxEarlyData).toBeUndefined();
      expect(cleared.wsSettings?.earlyDataHeaderName).toBeUndefined();
      expect(cleared.wsSettings?.path).toBe('/p'); // 同块 path 不受牵连
    });

    it(`${proto}: 切到 httpupgrade 不清空这两键（控件隐藏 ≠ 该删，后端那条腿也读不到）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'ws',
        wsSettings: { path: '/p', maxEarlyData: 2560, earlyDataHeaderName: 'X-Ed' },
      };
      const d = protoCodec[proto].fromConfig(base);
      d.net = 'httpupgrade';
      const out = protoCodec[proto].toConfig(d, base);
      expect(out.wsSettings?.maxEarlyData).toBe(2560);
      expect(out.wsSettings?.earlyDataHeaderName).toBe('X-Ed');
    });
  }

  it('谓词本身：whenWs 只认 ws（whenWsLike 还认 httpupgrade）', () => {
    expect(whenWs({ net: 'ws' })).toBe(true);
    expect(whenWs({ net: 'httpupgrade' })).toBe(false);
    expect(whenWsLike({ net: 'httpupgrade' })).toBe(true); // 阴性对照：两个谓词确实不同
    expect(whenWs({})).toBe(false);
  });
});

describe('snell：version 驱动 obfs（v4）/ mode（v6）互斥清空', () => {
  it('v4→v6 切换：obfsMode/obfsHost 清空，mode 按草稿写入', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'snell',
      password: 'psk',
      snellSettings: { version: 4, obfsMode: 'http', obfsHost: 'bing.com' },
    };
    const draft = protoCodec.snell.fromConfig(existing);
    draft.version = '6';
    draft.mode = 'unshaped';
    const out = protoCodec.snell.toConfig(draft, existing);
    expect(out.snellSettings?.version).toBe(6);
    expect(out.snellSettings?.obfsMode).toBeUndefined();
    expect(out.snellSettings?.obfsHost).toBeUndefined();
    expect(out.snellSettings?.mode).toBe('unshaped');
  });

  it('v6→v4 切换：mode 清空，obfsMode=none 时 obfsHost 不下发', () => {
    const existing: ServerConfig = {
      ...META,
      protocol: 'snell',
      password: 'psk',
      snellSettings: { version: 6, mode: 'unsafe-raw' },
    };
    const draft = protoCodec.snell.fromConfig(existing);
    draft.version = '4';
    draft.obfsMode = 'none';
    const out = protoCodec.snell.toConfig(draft, existing);
    expect(out.snellSettings?.version).toBe(4);
    expect(out.snellSettings?.mode).toBeUndefined();
    expect(out.snellSettings?.obfsMode).toBe('none');
    expect(out.snellSettings?.obfsHost).toBeUndefined();
  });
});

describe('custom：JSON 校验（镜像后端 store/validate.rs#protocol_requirement_ok）', () => {
  const base: ServerConfig = { ...META, protocol: 'custom' };

  it('非法 JSON → 抛错（不静默存半成品）', () => {
    const draft = protoCodec.custom.fromConfig(base);
    draft.outbound = '{not json';
    expect(() => protoCodec.custom.toConfig(draft, base)).toThrow(/JSON 格式错误/);
  });

  it('JSON 是数组/非对象 → 抛错', () => {
    const draft = protoCodec.custom.fromConfig(base);
    draft.outbound = '[1,2,3]';
    expect(() => protoCodec.custom.toConfig(draft, base)).toThrow(/必须是对象/);
  });

  it('缺少 type 字段 → 抛错', () => {
    const draft = protoCodec.custom.fromConfig(base);
    draft.outbound = '{"server":"a.com"}';
    expect(() => protoCodec.custom.toConfig(draft, base)).toThrow(/type/);
  });

  it('合法 JSON → 正常提交', () => {
    const draft = protoCodec.custom.fromConfig(base);
    draft.outbound = '{"type":"shadowtls","server":"a.com"}';
    const out = protoCodec.custom.toConfig(draft, base);
    expect(out.customSettings?.outbound.type).toBe('shadowtls');
  });
});

describe('parseNumberField（R2：清空归 undefined 非 0）', () => {
  it('空串 / 纯空白 → undefined', () => {
    expect(parseNumberField('')).toBeUndefined();
    expect(parseNumberField('   ')).toBeUndefined();
  });
  it('解析失败 → undefined（不硬塞 0）', () => {
    expect(parseNumberField('abc')).toBeUndefined();
    expect(parseNumberField('12abc')).toBeUndefined();
    expect(parseNumberField('Infinity')).toBeUndefined();
  });
  it('合法十进制 → 数值（含 0）', () => {
    expect(parseNumberField('0')).toBe(0);
    expect(parseNumberField('443')).toBe(443);
    expect(parseNumberField(' 8388 ')).toBe(8388);
  });
});

describe('toCselOptions（FieldSpec select → Csel 选项）', () => {
  /**
   * 牙：把映射改回 `([v, l]) => ({ value: v, label: l })` → 第三位被丢掉 → 本条转红。
   * `Csel` 早就支持 `disabled`（点击拦截 / 键盘跳过 / aria-disabled 全在），断点只在这一层，
   * 且因 node 环境渲染不了组件，除了这条断言没有任何门会发现。
   */
  it('第三位 disabled 原样传给 Csel', () => {
    expect(toCselOptions([['a', 'A'], ['b', 'B', true], ['c', 'C', false]])).toEqual([
      { value: 'a', label: 'A', disabled: undefined },
      { value: 'b', label: 'B', disabled: true },
      { value: 'c', label: 'C', disabled: false },
    ]);
  });

  it('二元组（全仓 22 处 select 调用点原封不动的形状）照旧可选', () => {
    const [only] = toCselOptions([['tcp', 'TCP']]);
    expect(only.value).toBe('tcp');
    expect(only.disabled).toBeUndefined();
  });

  /**
   * 未知当前值保留 —— 数据丢失防线，**不是**「选项集给几档」那个产品问题。
   * 选项集是前端选的展示档位，磁盘上的值域由 sing-box/后端拥有且更宽（Rust 侧 method/fingerprint
   * 都是开放 `String`）。存量值落在表外时若不并入，下拉是空选中态，用户一碰就被迫改成表内某档 ——
   * 静默改坏一个本来能用的节点。
   */
  it('当前值不在选项集内 → 并入首位（值即文案）', () => {
    const opts = toCselOptions([['a', 'A'], ['b', 'B']], 'aes-192-gcm');
    expect(opts[0]).toEqual({ value: 'aes-192-gcm', label: 'aes-192-gcm', disabled: undefined });
    expect(opts.map((o) => o.value)).toEqual(['aes-192-gcm', 'a', 'b']);
  });

  it('当前值已在选项集内 / 为空串 / 未传 → 选项集原样（不重复、不塞空项）', () => {
    const base = [['a', 'A'], ['b', 'B']] as const;
    expect(toCselOptions(base, 'b').map((o) => o.value)).toEqual(['a', 'b']);
    expect(toCselOptions(base, '').map((o) => o.value)).toEqual(['a', 'b']); // '' = 未设置，非未知取值
    expect(toCselOptions(base, undefined).map((o) => o.value)).toEqual(['a', 'b']);
    expect(toCselOptions(base, 42).map((o) => o.value)).toEqual(['a', 'b']); // 非字符串值域不参与
  });

  // 探针从 `aes-192-gcm` 换成 `salsa20`：ss 加密方式放开到内核全集（18 档）后，前者已进表内，
  // 拿它当「表外值」等于这条断言什么都不测了。`salsa20` 是老 shadowsocks 的流式密码，
  // **不在随包核 beta.7 的 method enum 里** —— 机场/存量配置里确实还有，正是本条要守的场景。
  it('端到端：存量 ss 节点的表外 method 经 fromConfig → 下拉里仍有该项（不被迫改值）', () => {
    const legacy: ServerConfig = {
      ...META,
      protocol: 'shadowsocks',
      shadowsocksSettings: { method: 'salsa20', password: 'p' }, // 内核 enum 之外
    };
    const draft = protoCodec.shadowsocks.fromConfig(legacy);
    expect(draft.method).toBe('salsa20');
    const methodField = allFields('shadowsocks').find((f) => f.k === 'method');
    const options = methodField && methodField.t === 'select' ? methodField.options : [];
    expect(options.map(([v]) => v)).not.toContain('salsa20'); // 前提：确实是表外值
    expect(toCselOptions(options, draft.method).map((o) => o.value)).toContain('salsa20');
  });

  /**
   * **接线门**：能力在 `toCselOptions` 里活着 ≠ `FieldRenderer` 真把当前值传了进去 —— `disabled` 正是
   * 这么一路从 `Csel`（早就支持）断在中间层、没有任何门发现的，这一层必须有自己的断言。
   *
   * 走 `renderToStaticMarkup` + `Csel` 替身（见文件头 `vi.mock`）：断言的是**下拉真正收到的选项集**。
   * 漏传当前值时，表外存量值不在其中 ⇒ 真机上就是一个空白选中态的下拉，用户一碰就被迫改值。
   */
  it('接线门：FieldRenderer 把当前值传给 toCselOptions（漏传则下拉里没有该项）', () => {
    const spec = allFields('shadowsocks').find((f) => f.k === 'method');
    expect(spec?.t).toBe('select');
    if (!spec) throw new Error('method 字段不存在');
    const optsOf = (value: string) => {
      const html = renderToStaticMarkup(
        createElement(FieldRenderer, { spec, value, onChange: () => {} })
      );
      return /data-opts="([^"]*)"/.exec(html)?.[1].split('|') ?? [];
    };
    // 阴性对照：表内值 → 选项集原样，不凭空多项（长度 = 内核 enum 的 18 档）。
    const inTable = optsOf('aes-256-gcm');
    expect(inTable).toHaveLength(18);
    expect(inTable[0]).toBe('2022-blake3-aes-128-gcm');
    expect(inTable).toContain('aes-256-gcm');
    // 正题：表外存量值并入首位。
    expect(optsOf('salsa20')[0]).toBe('salsa20');
    expect(optsOf('salsa20')).toHaveLength(19);
  });
});

/**
 * **必填字段不得落空串**（批 C：把「半假控件」这条从覆盖率门里挪过来）。
 *
 * # 这条断言守的是什么
 *
 * 覆盖率门（`contracts/protocol-settings-coverage.test.ts`）只管「这个键在这个协议下有没有编辑入口」，
 * 管不了「开关一开就写出一个用户填不满的空壳」。ShadowTLS 那条缺陷正是后者：`stls` 开关一打开，
 * 旧 codec 就无条件写 `{ password: '', sni: '' }`，而当时表单上没有这两颗控件 ⇒ 后端
 * `apply_shadow_tls_postprocess` 只看 `is_some()`，照样造出外层 shadowtls 出站并把 SS 的 detour
 * 指过去，节点必然连不上、UI 上又没有任何字段能修。**随包核 `sing-box check` 对这份配置 exit=0**
 * （空口令是合法 JSON），后端零防线 ⇒ 前端的「齐备才写」是唯一的门，那这道门就得有牙。
 *
 * # 判据（不手抄清单，从 Rust 结构体机器推导）
 *
 * Rust 侧**非 `Option` 的 `String` 字段**就是「这个块存在就必须有值」的字段（`serde` 反序列化时缺了
 * 直接报错，给空串则静默造出坏节点）。逐协议把草稿**按控件填满**后跑 `toConfig`，产出的每个嵌套
 * settings 块里，这些字段既不许缺席、也不许是空串。
 *
 * 「按控件填满」是判据的关键：**填的只有表里真有的控件**。某个必填字段若压根没有控件，就没有任何
 * 草稿值能喂给它 ⇒ 它必然落空 ⇒ 红。反过来，有控件的字段由 NodeDialog 的必填校验兜着，不该在这里
 * 二次要求。这条规则把「半假控件」精确地圈了出来，不误伤「用户没填必填项」。
 */
const RUST_PS = readFileSync(
  fileURLToPath(new URL('../../../../crates/config-engine/src/user_config/protocol_settings.rs', import.meta.url)),
  'utf8'
);
const RUST_SC = readFileSync(
  fileURLToPath(new URL('../../../../crates/config-engine/src/user_config/server_config.rs', import.meta.url)),
  'utf8'
);

/** 剔注释（同覆盖率门的理由：doc 注释里的 `pub foo:` 会解析出幽灵字段）。 */
const rmComments = (s: string): string => s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');

/** `pub struct <Name> { … }` 的体（花括号配对）。 */
function rustStructBody(src: string, name: string): string {
  const s = rmComments(src);
  const at = s.indexOf(`pub struct ${name} {`);
  expect(at, `Rust 侧 pub struct ${name} 解析失败 —— 解析不到必须转红`).toBeGreaterThanOrEqual(0);
  const open = s.indexOf('{', at);
  let depth = 0;
  for (let i = open; i < s.length; i++) {
    if (s[i] === '{') depth++;
    else if (s[i] === '}') {
      depth--;
      if (depth === 0) return s.slice(open + 1, i);
    }
  }
  throw new Error(`pub struct ${name} 花括号不配对`);
}

/** 字段名 → JSON 键（有 `#[serde(rename)]` 取 rename）。逐字段只看「上一字段到本字段」之间的属性。 */
function jsonKeyOf(body: string, decls: { name: string; at: number }[], i: number): string {
  const attrs = body.slice(i === 0 ? 0 : decls[i - 1].at, decls[i].at);
  return /rename\s*=\s*"([^"]+)"/.exec(attrs)?.[1] ?? decls[i].name;
}

/** 某结构体里**非 Option 的 String** 字段的 JSON 键集（= 该块存在就必须有值的字段）。 */
function requiredStringKeys(src: string, struct: string): string[] {
  const body = rustStructBody(src, struct);
  const decls = [...body.matchAll(/pub\s+(\w+)\s*:\s*([^,]+),/g)].map((m) => ({
    name: m[1],
    ty: m[2].trim(),
    at: m.index as number,
  }));
  expect(decls.length, `${struct} 没解析出任何字段 —— 解析器失效`).toBeGreaterThan(0);
  return decls.map((d, i) => (d.ty === 'String' ? jsonKeyOf(body, decls, i) : '')).filter(Boolean);
}

/** ServerConfig 上「结构体 → JSON 字段名」（`ShadowTlsSettings` → `shadowTlsSettings`）。 */
const BLOCK_OF: ReadonlyMap<string, string> = (() => {
  const body = rustStructBody(RUST_SC, 'ServerConfig');
  const decls = [...body.matchAll(/pub\s+(\w+)\s*:\s*([^,\n]+),/g)].map((m) => ({
    name: m[1],
    ty: m[2],
    at: m.index as number,
  }));
  const out = new Map<string, string>();
  decls.forEach((d, i) => {
    const struct = /\b(\w+Settings)\s*>/.exec(d.ty)?.[1];
    if (struct && !out.has(struct)) out.set(struct, jsonKeyOf(body, decls, i));
  });
  return out;
})();

/** 全部「块 + 必填键」组合（机器推导，改 Rust 就跟着变）。 */
const REQUIRED_IN_BLOCK: ReadonlyArray<{ struct: string; block: string; keys: readonly string[] }> = [
  ...new Set([...RUST_PS.matchAll(/pub struct (\w+Settings)\s*\{/g)].map((m) => m[1])),
]
  .map((struct) => ({ struct, block: BLOCK_OF.get(struct) ?? '', keys: requiredStringKeys(RUST_PS, struct) }))
  .filter((r) => r.keys.length > 0);

/** 产出的 config 里，哪些「必填 String」缺席或落了空串。 */
function emptyRequired(cfg: ServerConfig): string[] {
  const bad: string[] = [];
  for (const { struct, block, keys } of REQUIRED_IN_BLOCK) {
    const v = (cfg as unknown as Record<string, unknown>)[block];
    if (v === undefined || v === null || typeof v !== 'object') continue;
    for (const k of keys) {
      const got = (v as Record<string, unknown>)[k];
      if (typeof got !== 'string' || got.trim() === '') bad.push(`${struct}.${k}`);
    }
  }
  return bad;
}

/** 草稿默认值之外的填充值（其余文本框一律 'x'）。 */
const DUMMY: Record<string, FormValue> = { outbound: '{"type":"vless","server":"e.com","server_port":443}' };

/** 把该协议**表里真有的控件**全部填满：文本 → 非空、数字 → 1、开关 → 开、下拉 → 首项（可覆写）。 */
function filledDraft(proto: NodeProto, override: Record<string, FormValue> = {}): FormValues {
  const d = draftFromSpecs(allFields(proto));
  for (const f of allFields(proto)) {
    if (f.t === 'switch') d[f.k] = true;
    else if (f.t === 'number') d[f.k] = 1;
    else if (f.t === 'select') d[f.k] = f.options[0][0];
    else d[f.k] = DUMMY[f.k] ?? 'x';
  }
  return { ...d, ...override };
}

describe('必填字段不得落空串（半假控件门；ShadowTLS 那类空壳的通用形态）', () => {
  it('必填清单是从 Rust 结构体推导出来的，且非空（推导塌了 = 这道门空转）', () => {
    const flat = REQUIRED_IN_BLOCK.flatMap((r) => r.keys.map((k) => `${r.struct}.${k}`));
    // 落地时的实测值：ShadowTLS 的 password/sni 正是当初那条坏节点缺陷的两个字段。
    expect(flat).toContain('ShadowTlsSettings.password');
    expect(flat).toContain('ShadowTlsSettings.sni');
    expect(flat).toContain('ShadowsocksSettings.method');
    expect(flat).toContain('ShadowsocksSettings.password');
    expect(flat).toContain('RealitySettings.publicKey');
    // 每个结构体都要挂得上 ServerConfig 的某个 JSON 字段，否则 `emptyRequired` 根本走不到它。
    for (const r of REQUIRED_IN_BLOCK) {
      expect(r.block, `${r.struct} 在 ServerConfig 上没解析到 JSON 字段名 —— 这道门看不见它`).not.toBe('');
    }
  });

  it('本断言确有牙（正向对照：喂一个空壳块，检查器必须点名）', () => {
    const shell = {
      ...META,
      protocol: 'shadowsocks',
      shadowTlsSettings: { password: '', sni: '' },
    } as unknown as ServerConfig;
    expect(emptyRequired(shell)).toEqual(['ShadowTlsSettings.password', 'ShadowTlsSettings.sni']);
    // 阴性对照：填满就干净，避免「恒报错」式的假牙。
    const ok = {
      ...META,
      protocol: 'shadowsocks',
      shadowTlsSettings: { password: 'p', sni: 's' },
    } as unknown as ServerConfig;
    expect(emptyRequired(ok)).toEqual([]);
  });

  it.each(PROTO_OPTIONS.map(([v]) => v))(
    '%s：控件填满后 toConfig 产出的每个嵌套块都没有空的必填字段',
    (proto) => {
      const base = { ...META, protocol: proto } as ServerConfig;
      // 基准 + 逐个下拉遍历它的每一档（sec='reality'、obfs='gecko' 这类分支才走得到）。
      const drafts: FormValues[] = [filledDraft(proto)];
      for (const f of allFields(proto)) {
        if (f.t !== 'select') continue;
        for (const [value] of f.options) drafts.push(filledDraft(proto, { [f.k]: value }));
      }
      for (const draft of drafts) {
        const out = protoCodec[proto].toConfig(draft, base);
        expect(
          emptyRequired(out),
          `${proto}：某个 Rust 侧必填（非 Option 的 String）字段落了空/空串 —— ` +
            `多半是「开关一开就写块、却没有能填它的控件」那类半假控件（草稿：${JSON.stringify(draft)}）`
        ).toEqual([]);
      }
    }
  );
});

// ── 批 D：h2 传输四件套 · alpn×5 · http 指纹 · hy2 network · naive ECH · fragment×5 ──────────

describe('HTTP/2 传输四件套（httpSettings；vless / vmess / trojan）', () => {
  const T_PROTOS = ['vless', 'vmess', 'trojan'] as const;
  const H2_KEYS = ['h2Path', 'h2Host', 'h2Method', 'h2Headers'] as const;

  for (const proto of T_PROTOS) {
    it(`${proto}: 四颗控件齐备且只在 net=http 下可见（正向对照逐档取值）`, () => {
      const fields = allFields(proto);
      for (const k of H2_KEYS) {
        expect(fields.find((f) => f.k === k), `${proto} 缺少 ${k} 控件`).toBeDefined();
        expect(fields.find((f) => f.k === k)?.when, `${proto}.${k} 必须带门`).toBeDefined();
      }
      const draft = protoCodec[proto].fromConfig(SAMPLES[proto]);
      // 🔴 正向对照：谓词读的草稿键真的存在。读成 `network`（snell 的键）会恒 false、控件永不渲染。
      expect(draft.net, `${proto} 草稿里必须有 net 键，否则 whenH2 恒 false`).toBeDefined();
      const visibleAt = (net: string): string[] => {
        draft.net = net;
        return H2_KEYS.filter((k) => fields.find((f) => f.k === k)?.when?.(draft));
      };
      expect(visibleAt('http')).toEqual([...H2_KEYS]);
      // 阴性对照：别的传输档一个都不显（否则就是恒真谓词）。
      expect(visibleAt('ws')).toEqual([]);
      expect(visibleAt('httpupgrade')).toEqual([]);
      expect(visibleAt('grpc')).toEqual([]);
      expect(visibleAt('tcp')).toEqual([]);
    });

    it(`${proto}: 回填 + 下发 + 往返对称（host 逗号分隔 ⇄ 数组，headers 多行 ⇄ Record）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'http',
        httpSettings: {
          path: '/h2',
          host: ['a.com', 'b.com'],
          method: 'PUT',
          headers: { 'X-Real-IP': ['1.2.3.4'] },
        },
      };
      const d = protoCodec[proto].fromConfig(base);
      expect(d.net).toBe('http');
      expect(d.h2Path).toBe('/h2');
      expect(d.h2Host).toBe('a.com,b.com');
      expect(d.h2Method).toBe('PUT');
      expect(d.h2Headers).toBe('X-Real-IP: 1.2.3.4');

      const out = protoCodec[proto].toConfig(d, base);
      expect(out.httpSettings).toEqual(base.httpSettings);
      // 往返恒等（第二圈）：回填 → 提交 应逐字段稳定。
      expect(protoCodec[proto].toConfig(protoCodec[proto].fromConfig(out), out).httpSettings).toEqual(
        base.httpSettings
      );
    });

    it(`${proto}: 同名头多行合并成一组值（Rust 侧是 Vec<String>，不是后者覆盖前者）`, () => {
      const base: ServerConfig = { ...SAMPLES[proto], network: 'http' };
      const d = protoCodec[proto].fromConfig(base);
      d.net = 'http';
      d.h2Headers = 'X-Tag: a\nX-Tag: b\nX-Other: c';
      const out = protoCodec[proto].toConfig(d, base);
      expect(out.httpSettings?.headers).toEqual({ 'X-Tag': ['a', 'b'], 'X-Other': ['c'] });
      // 往返：多值头再序列化回多行。
      expect(protoCodec[proto].fromConfig(out).h2Headers).toBe('X-Tag: a\nX-Tag: b\nX-Other: c');
    });

    it(`${proto}: 四键留空 → 整块不下发（默认不填 ⇒ 产物不变，金样零变化的那一格）`, () => {
      const base: ServerConfig = { ...SAMPLES[proto], network: 'http', httpSettings: undefined };
      const d = protoCodec[proto].fromConfig(base);
      d.net = 'http';
      expect(protoCodec[proto].toConfig(d, base).httpSettings).toBeUndefined();
    });

    it(`${proto}: 脏输入按 Rust 空值语义落删键（不写 [] / 不写空值头）`, () => {
      const base: ServerConfig = { ...SAMPLES[proto], network: 'http' };
      const d = protoCodec[proto].fromConfig(base);
      d.net = 'http';
      d.h2Host = ' , , ';           // 只剩分隔符 —— listFromText 必须落回删键而不是 []
      d.h2Headers = '没有冒号的行\n: 头名为空\nX-Empty:   ';  // 三种都该被丢弃
      d.h2Path = '   ';
      d.h2Method = '';
      expect(protoCodec[proto].toConfig(d, base).httpSettings).toBeUndefined();
    });

    it(`${proto}: 切走 h2 → 整块清除（同 ws/grpc 的「不匹配当前传输就清」）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        network: 'http',
        httpSettings: { path: '/h2', host: ['a.com'] },
      };
      const d = protoCodec[proto].fromConfig(base);
      d.net = 'ws';
      expect(protoCodec[proto].toConfig(d, base).httpSettings).toBeUndefined();
    });
  }

  it('谓词本身：whenH2 只认 http，且与 whenWsLike / whenGrpc 互斥', () => {
    expect(whenH2({ net: 'http' })).toBe(true);
    expect(whenH2({ net: 'ws' })).toBe(false);
    expect(whenH2({ net: 'httpupgrade' })).toBe(false);
    expect(whenH2({})).toBe(false);
    // 阴性对照：三个传输谓词在 http 档下只有一个为真（否则会同时露出两组控件）。
    expect([whenH2, whenWsLike, whenGrpc].filter((f) => f({ net: 'http' })).length).toBe(1);
  });

  it('http **协议**（≠h2 传输）刻意没有这四颗控件 —— 后端那条腿产出的是内核拒绝加载的配置', () => {
    // `Protocol::Http` 分支把 headers/path 塞进 `ob.transport`，而随包核 beta.7 的 http 出站
    // schema 无 `transport` 键且 additionalProperties:false ⇒ 真下发 FATAL。见覆盖门 PORT_DEBT 注释。
    const fields = allFields('http').map((f) => f.k);
    for (const k of ['h2Path', 'h2Host', 'h2Method', 'h2Headers']) {
      expect(fields, `http 协议表单不得有 ${k}（会造出核起不来的节点）`).not.toContain(k);
    }
  });
});

describe('ALPN 跨协议统一（vless / vmess / trojan / anytls / http / hysteria2）', () => {
  const A_PROTOS = ['vless', 'vmess', 'trojan', 'anytls', 'http', 'hysteria2'] as const;

  for (const proto of A_PROTOS) {
    it(`${proto}: 有 alpn 控件；回填 + 下发 + 往返`, () => {
      expect(allFields(proto).find((f) => f.k === 'alpn'), `${proto} 缺少 alpn 控件`).toBeDefined();
      const base: ServerConfig = {
        ...SAMPLES[proto],
        tlsSettings: { ...SAMPLES[proto].tlsSettings, alpn: ['h2', 'http/1.1'] },
      };
      const d = protoCodec[proto].fromConfig(base);
      expect(d.alpn).toBe('h2,http/1.1');
      const out = protoCodec[proto].toConfig(d, base);
      expect(out.tlsSettings?.alpn).toEqual(['h2', 'http/1.1']);
      expect(protoCodec[proto].fromConfig(out).alpn).toBe('h2,http/1.1');
    });

    it(`${proto}: 留空 / 纯逗号 → 删键（**不写空数组**：trojan 的后端缺省会被顶掉）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        tlsSettings: { ...SAMPLES[proto].tlsSettings, alpn: ['h2'] },
      };
      const d = protoCodec[proto].fromConfig(base);
      for (const dirty of ['', '   ', ' , , ']) {
        d.alpn = dirty;
        const out = protoCodec[proto].toConfig(d, base);
        expect(out.tlsSettings?.alpn, `${proto} alpn=${JSON.stringify(dirty)} 必须删键`).toBeUndefined();
      }
    });

    it(`${proto}: 不归一大小写（ALPN 名对内核大小写敏感，h2 ≠ H2）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        tlsSettings: { ...SAMPLES[proto].tlsSettings, alpn: ['H2'] },
      };
      expect(protoCodec[proto].fromConfig(base).alpn).toBe('H2');
    });
  }

  it('tuic 的 alpn 也改走 listFromText（`809f476` 漏在射程外的同一个空数组缺陷）', () => {
    const base: ServerConfig = { ...SAMPLES.tuic, tlsSettings: { alpn: ['h3'] } };
    const d = protoCodec.tuic.fromConfig(base);
    d.alpn = ' , , ';
    expect(protoCodec.tuic.toConfig(d, base).tlsSettings?.alpn).toBeUndefined();
  });

  it('trojan 的占位符仍是后端专属缺省 http/1.1（其余协议不是）', () => {
    const alpnPh = (proto: NodeProto): string | undefined => {
      const f = allFields(proto).find((x) => x.k === 'alpn');
      return f !== undefined && f.t === 'text' ? f.ph : undefined;
    };
    expect(alpnPh('trojan')).toBe('http/1.1');
    expect(alpnPh('vless')).toBe('h2,http/1.1');
    expect(alpnPh('hysteria2')).toBe('h3');
  });
});

describe('TLS 分片 fragment（vless / vmess / trojan / anytls / http）', () => {
  const F_PROTOS = ['vless', 'vmess', 'trojan', 'anytls', 'http'] as const;

  for (const proto of F_PROTOS) {
    it(`${proto}: 有开关；开 → 下发 true；关 → 删键（**不写 false**）`, () => {
      const spec = allFields(proto).find((f) => f.k === 'fragment');
      expect(spec, `${proto} 缺少 fragment 开关`).toBeDefined();
      expect(spec?.t).toBe('switch');

      const base: ServerConfig = { ...SAMPLES[proto] };
      const d = protoCodec[proto].fromConfig(base);
      d.fragment = true;
      expect(protoCodec[proto].toConfig(d, base).tlsSettings?.fragment).toBe(true);

      d.fragment = false;
      const off = protoCodec[proto].toConfig(d, base);
      expect(off.tlsSettings?.fragment).toBeUndefined();
      expect(Object.keys(off.tlsSettings ?? {}), `${proto} 关掉时不得留 fragment 键`).not.toContain(
        'fragment'
      );
    });

    it(`${proto}: 存量 fragment:true 经编辑保全（回填 → 提交往返恒等）`, () => {
      const base: ServerConfig = {
        ...SAMPLES[proto],
        tlsSettings: { ...SAMPLES[proto].tlsSettings, fragment: true },
      };
      const d = protoCodec[proto].fromConfig(base);
      expect(d.fragment).toBe(true);
      expect(protoCodec[proto].toConfig(d, base).tlsSettings?.fragment).toBe(true);
    });

    it(`${proto}: 默认草稿不下发 fragment（金样零变化）`, () => {
      const d = draftFromSpecs(allFields(proto));
      expect(d.fragment).toBe(false);
    });
  }

  it('门是一级门、**不叠 !whenReality**（与 engine 的分歧点：fragment 在 reality 下照常生效）', () => {
    const fields = allFields('vless');
    const d = protoCodec.vless.fromConfig(SAMPLES.vless);
    const vis = (sec: string, k: string): boolean => {
      d.sec = sec;
      return fields.find((f) => f.k === k)?.when?.(d) === true;
    };
    expect(vis('reality', 'fragment')).toBe(true);   // fragment 在 reality 下仍显
    expect(vis('reality', 'engine')).toBe(false);    // 阴性对照：engine 被吞，故隐藏
    expect(vis('tls', 'fragment')).toBe(true);
    expect(vis('none', 'fragment')).toBe(false);     // 一级门仍然管用
  });

  it('QUIC 三协议（hy2 / tuic / naive）刻意没有这颗开关 —— 后端 fragment_unsupported 挡着', () => {
    for (const proto of ['hysteria2', 'tuic', 'naive'] as const) {
      expect(allFields(proto).map((f) => f.k), `${proto} 不该有 fragment`).not.toContain('fragment');
    }
  });
});

describe('http 协议的 uTLS 指纹（此前 5 协议里唯一漏的那一个）', () => {
  it('取值集必须是带空首项的 O_FP_OPT —— 用 O_FP 会让新建节点凭空多出 utls 块', () => {
    const spec = allFields('http').find((f) => f.k === 'fp');
    expect(spec, 'http 缺少 fp 控件').toBeDefined();
    expect(spec?.t).toBe('select');
    // 🔴 select 首项陷阱：draftFromSpecs 把首项当默认 seed。后端对 http 的 final_fp 缺省是 none
    //    ⇒ 首项必须是「不下发」语义的空串（vless/anytls 缺省 chrome，故它们才可以用 O_FP）。
    expect(spec?.t === 'select' ? spec.options[0][0] : null).toBe('');
    expect(draftFromSpecs(allFields('http')).fp).toBe('');
  });

  it('新建 http 节点不下发 fingerprint；选了档位才下发并往返', () => {
    const base: ServerConfig = { ...SAMPLES.http, security: 'tls', tlsSettings: {} };
    const d = protoCodec.http.fromConfig(base);
    d.tls = true;
    d.fp = '';
    expect(protoCodec.http.toConfig(d, base).tlsSettings?.fingerprint).toBeUndefined();
    d.fp = 'firefox';
    const out = protoCodec.http.toConfig(d, base);
    expect(out.tlsSettings?.fingerprint).toBe('firefox');
    expect(protoCodec.http.fromConfig(out).fp).toBe('firefox');
  });

  it('门是 whenHttpTls（读 tls 开关）——关掉 TLS 时整块清除', () => {
    const fields = allFields('http');
    const d = protoCodec.http.fromConfig(SAMPLES.http);
    d.tls = true;
    expect(fields.find((f) => f.k === 'fp')?.when?.(d)).toBe(true);
    d.tls = false;
    expect(fields.find((f) => f.k === 'fp')?.when?.(d)).toBe(false);
  });

  it('R4：存量大写指纹归一（后端消费点是精确比较）', () => {
    const base: ServerConfig = { ...SAMPLES.http, security: 'tls', tlsSettings: { fingerprint: 'Chrome' } };
    expect(protoCodec.http.fromConfig(base).fp).toBe('chrome');
  });
});

describe('hysteria2 的 network（被 snell 的 {k:"network"} 遮蔽过的那一条）', () => {
  it('首项是空串 = 不下发该键（内核缺省 tcp+udp 都走）', () => {
    const spec = allFields('hysteria2').find((f) => f.k === 'network');
    expect(spec, 'hy2 缺少 network 控件').toBeDefined();
    expect(spec?.t === 'select' ? spec.options.map(([v]) => v) : null).toEqual(['', 'tcp', 'udp']);
    expect(draftFromSpecs(allFields('hysteria2')).network).toBe('');
  });

  it('留空 → 删键；选单侧 → 下发并往返', () => {
    const base: ServerConfig = { ...SAMPLES.hysteria2 };
    const d = protoCodec.hysteria2.fromConfig(base);
    d.network = '';
    expect(protoCodec.hysteria2.toConfig(d, base).hysteria2Settings?.network).toBeUndefined();
    for (const want of ['tcp', 'udp'] as const) {
      d.network = want;
      const out = protoCodec.hysteria2.toConfig(d, base);
      expect(out.hysteria2Settings?.network).toBe(want);
      expect(protoCodec.hysteria2.fromConfig(out).network).toBe(want);
    }
  });

  it('R3：存量大写值归一（内核 enum 只认小写）', () => {
    const base: ServerConfig = {
      ...SAMPLES.hysteria2,
      hysteria2Settings: { ...SAMPLES.hysteria2.hysteria2Settings, network: 'TCP' as 'tcp' },
    };
    expect(protoCodec.hysteria2.fromConfig(base).network).toBe('tcp');
  });
});

describe('naive 的 ECH（批 C 记成债务，批 D 实测坐实到得了内核）', () => {
  it('两颗控件齐备；echConfig 挂 whenEch（正向对照：开关是真实草稿键，不恒 false）', () => {
    const fields = allFields('naive');
    expect(fields.find((f) => f.k === 'ech'), 'naive 缺少 ech 开关').toBeDefined();
    expect(fields.find((f) => f.k === 'echConfig'), 'naive 缺少 echConfig').toBeDefined();
    const d = protoCodec.naive.fromConfig(SAMPLES.naive);
    expect(d.ech, 'naive 草稿里必须有 ech 键，否则 whenEch 恒 false').toBeDefined();
    d.ech = true;
    expect(fields.find((f) => f.k === 'echConfig')?.when?.(d)).toBe(true);
    d.ech = false;
    expect(fields.find((f) => f.k === 'echConfig')?.when?.(d)).toBe(false);
    // ech 开关本身无门（naive TLS 恒开，本表单没有 sec/tls 键）。
    expect(fields.find((f) => f.k === 'ech')?.when).toBeUndefined();
  });

  it('开 → 下发 ech + echConfig 并往返；关 → 两键都删（不写 false）', () => {
    const base: ServerConfig = { ...SAMPLES.naive };
    const d = protoCodec.naive.fromConfig(base);
    d.ech = true;
    d.echConfig = '-----BEGIN ECH CONFIGS-----\nAAAA\n-----END ECH CONFIGS-----';
    const out = protoCodec.naive.toConfig(d, base);
    expect(out.tlsSettings?.ech).toBe(true);
    expect(out.tlsSettings?.echConfig).toContain('BEGIN ECH CONFIGS');
    const back = protoCodec.naive.fromConfig(out);
    expect(back.ech).toBe(true);
    expect(back.echConfig).toBe(d.echConfig);

    d.ech = false;
    const off = protoCodec.naive.toConfig(d, base);
    expect(off.tlsSettings?.ech).toBeUndefined();
    expect(off.tlsSettings?.echConfig).toBeUndefined();
  });

  it('naive 仍然只建模 serverName + ECH —— 其余 TLS 项随包核会点名 FATAL，不得有控件', () => {
    const keys = allFields('naive').map((f) => f.k);
    for (const k of ['alpn', 'insecure', 'fp', 'engine', 'fragment', 'spoofMethod', 'spoofSni']) {
      expect(keys, `naive 不得有 ${k} 控件（内核 "… is not supported on naive outbound"）`).not.toContain(k);
    }
  });

  it('默认草稿不下发 ech（金样零变化）', () => {
    expect(draftFromSpecs(allFields('naive')).ech).toBe(false);
  });
});

describe('透传袋入口：表单必须够得到未建模字段', () => {
  // 缺陷原型（2026-08-11）：表单是精选子集，其余键此前**只有从本地文件导入**才进得了袋子，
  // 手建节点根本够不到 —— 等于「支持 AnyConnect 全部能力」只对导入成立。
  // openconnect 内核那支 61 个键，表单给 13；剩下的 csd / cookie / compression_mode …
  // 必须能从这一个控件写进去，且不必改用「自定义」协议（那会丢掉本协议的表单与校验）。
  const BAG_PROTOS = ['openconnect', 'openvpn-client', 'hysteria', 'tor'] as const;

  for (const proto of BAG_PROTOS) {
    it(`${proto}：extraJson 写入的键活着进设置，且同名时具名字段压过袋子`, () => {
      const base = { ...SAMPLES[proto] };
      const draft = protoCodec[proto].fromConfig(base);
      draft.extraJson = JSON.stringify({ csd: '/usr/lib/csd-wrapper.sh', dpd_interval: '30s' });
      const out = protoCodec[proto].toConfig(draft, base) as unknown as Record<string, unknown>;
      const key = {
        openconnect: 'openconnectSettings',
        'openvpn-client': 'openvpnClientSettings',
        hysteria: 'hysteriaSettings',
        tor: 'torSettings',
      }[proto];
      const settings = out[key] as Record<string, unknown>;
      expect(settings.csd, '袋子里的键没进设置 —— 手建节点仍够不到未建模字段').toBe(
        '/usr/lib/csd-wrapper.sh'
      );
      expect(settings.dpd_interval).toBe('30s');
    });

    it(`${proto}：extraJson 是坏 JSON 时保留旧袋，不静默清空`, () => {
      const base = { ...SAMPLES[proto] };
      const draft = protoCodec[proto].fromConfig(base);
      draft.extraJson = '{ 这不是 JSON';
      // 不抛异常即可 —— 用户手误不该让保存崩掉，也不该把已有的袋子清空。
      expect(() => protoCodec[proto].toConfig(draft, base)).not.toThrow();
    });
  }
});


describe('torrc 控件：原生语法 ⇄ 键值表', () => {
  const draftOf = () => protoCodec.tor.fromConfig(SAMPLES.tor);
  const torrcOf = (text: string) =>
    (protoCodec.tor.toConfig({ ...draftOf(), torrcText: text }, SAMPLES.tor) as ServerConfig)
      .torSettings?.torrc;

  it('每行 `Key Value`；值里的空格保留（如 ExitNodes {jp},{us}）', () => {
    expect(torrcOf('ExitNodes {jp},{us}\nStrictNodes 1')).toEqual({
      ExitNodes: '{jp},{us}',
      StrictNodes: '1',
    });
    expect(torrcOf('Log notice file /var/log/tor.log')).toEqual({
      Log: 'notice file /var/log/tor.log',
    });
  });

  it('空行与 # 注释丢弃 —— 内核侧是 map，承载不了它们（不是本控件的损失）', () => {
    expect(torrcOf('# 这是注释\n\nExitNodes {jp}\n   \n')).toEqual({ ExitNodes: '{jp}' });
  });

  it('无值的裸键 → 空串（torrc 里确有此形态，如 AvoidDiskWrites）', () => {
    expect(torrcOf('AvoidDiskWrites')).toEqual({ AvoidDiskWrites: '' });
  });

  it('往返恒等：map → 文本 → map', () => {
    const d = draftOf();
    expect(torrcOf(d.torrcText as string)).toEqual(SAMPLES.tor.torSettings?.torrc);
  });
});

describe('endpoint 腿 VPN 客户端的内网段与全隧道开关', () => {
  it('meshRoutes 落 ServerConfig **顶层**，不进 settings 块', () => {
    // 那两个 settings 块的键名 = sing-box 键名、整体 flatten 下发；混进一个内核不认的键会硬报错。
    const cfg = protoCodec.openconnect.toConfig(
      { meshRoutes: '10.10.0.0/16\n  \n192.168.1.0/24' },
      { id: 'x', name: 'X', protocol: 'openconnect', address: 'vpn.example.com', port: 443 } as ServerConfig
    );
    expect(cfg.meshRoutes).toEqual(['10.10.0.0/16', '192.168.1.0/24']);
    expect(JSON.stringify(cfg.openconnectSettings)).not.toContain('meshRoutes');
  });

  it('OpenConnect server 只由公共地址/端口派生，IPv6 自动补方括号', () => {
    const base = { id: 'x', name: 'X', protocol: 'openconnect', address: '2001:db8::1', port: 4443 } as ServerConfig;
    expect(protoCodec.openconnect.toConfig({}, base).openconnectSettings?.server).toBe('[2001:db8::1]:4443');
  });

  it('OpenVPN 根级与 TLS 扩展使用两个独立透传袋', () => {
    const base = {
      id: 'x', name: 'X', protocol: 'openvpn-client', address: 'vpn.example.com', port: 1194,
      openvpnClientSettings: {
        tls: { certificate: ['CA'], server_name: 'old.example.com' },
      },
    } as ServerConfig;
    const cfg = protoCodec['openvpn-client'].toConfig(
      {
        ovpnCa: 'CA',
        extraJson: '{"route_no_pull":true,"server":"must-not-win.example"}',
        ovpnTlsExtraJson: '{"server_name":"new.example.com","certificate":["must-not-win"]}',
      },
      base
    );
    expect(cfg.openvpnClientSettings?.route_no_pull).toBe(true);
    expect(cfg.openvpnClientSettings?.tls?.server_name).toBe('new.example.com');
    expect(cfg.openvpnClientSettings?.tls?.route_no_pull).toBeUndefined();
    expect(cfg.openvpnClientSettings?.server).toBe('vpn.example.com');
    expect(cfg.openvpnClientSettings?.tls?.certificate).toEqual(['CA']);
  });

  it('meshRoutes 往返无损；空文本 → 删键而非空数组', () => {
    const base = { id: 'x', name: 'X', protocol: 'openconnect', address: '', port: 0 } as ServerConfig;
    const withRoutes = { ...base, meshRoutes: ['10.10.0.0/16'] };
    expect(protoCodec.openconnect.fromConfig(withRoutes).meshRoutes).toBe('10.10.0.0/16');
    expect(protoCodec.openconnect.toConfig({ meshRoutes: '   ' }, base).meshRoutes).toBeUndefined();
  });

  it('OpenVPN 全隧道开关关闭时**显式写 false**', () => {
    // 写 undefined 会让 `meshAllowsInternet` 按缺省判「承载全隧道」⇒ 用户关了开关，
    // 「只走声明的内网段、其余直连」那条兜底却不生效。
    const base = { id: 'x', name: 'X', protocol: 'openvpn-client', address: 'v.example.com', port: 1194 } as ServerConfig;
    expect(protoCodec['openvpn-client'].toConfig({ redirectGw: false }, base).openvpnClientSettings?.redirect_gateway).toBe(false);
    expect(protoCodec['openvpn-client'].toConfig({ redirectGw: true }, base).openvpnClientSettings?.redirect_gateway).toBe(true);
  });
});
