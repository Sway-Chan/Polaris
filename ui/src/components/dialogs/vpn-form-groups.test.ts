import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { allFields, ND_SPEC } from './node-spec';
import { WG_FORM_GROUP_KEYS, vpnDraftError } from './vpn-form-layout';

describe('VPN form information architecture', () => {
  it.each(['openconnect', 'openvpn-client'] as const)('%s uses three task-oriented groups without duplicating fields', (protocol) => {
    const groups = ND_SPEC[protocol].groups;
    expect(groups?.map((group) => group.id)).toEqual(['basic', 'routing', 'advanced']);
    const keys = allFields(protocol).map((field) => field.k);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it('WireGuard splits connection, routing and low-frequency fields', () => {
    expect(WG_FORM_GROUP_KEYS.basic).toEqual([
      'address', 'port', 'privateKey', 'localAddress', 'peerPublicKey', 'preSharedKey',
    ]);
    expect(WG_FORM_GROUP_KEYS.routing).toEqual([
      'allowedIPs', 'reverseMesh', 'allowInternet', 'alwaysRouteSubnets',
    ]);
    expect(WG_FORM_GROUP_KEYS.advanced).toEqual([
      'persistentKeepalive', 'mtu', 'reserved', 'detour',
    ]);
  });

  it('required and JSON errors point to the tab that can fix them', () => {
    expect(vpnDraftError('openconnect', {},)).toEqual({ tab: 'basic', key: 'required' });
    expect(vpnDraftError('openvpn-client', { user: 'u', pwd: 'p', ovpnCa: 'CA', extraJson: '[]' }))
      .toEqual({ tab: 'advanced', key: 'json' });
    expect(vpnDraftError('openvpn-client', { user: 'u', pwd: 'p', ovpnCa: 'CA', extraJson: '{}' }))
      .toBeNull();
  });

  it('统一添加菜单下的实际录入表单共用 620px，接入方式选择器单独使用 700px', () => {
    const read = (name: string) =>
      readFileSync(fileURLToPath(new URL(`./${name}`, import.meta.url)), 'utf8');
    for (const name of [
      'NodeDialog.tsx',
      'WgDialog.tsx',
      'WarpDialog.tsx',
      'TsLoginDialog.tsx',
      'TsSettingsDialog.tsx',
      'SubDialog.tsx',
      'ImportDialog.tsx',
    ]) {
      expect(read(name), `${name} 没有使用统一录入表单宽度`).toContain('entry-form-dlg');
    }
    expect(read('MeshJoinDialog.tsx')).toContain('access-picker-dlg');

    const css = readFileSync(
      fileURLToPath(new URL('../../styles/index.css', import.meta.url)),
      'utf8',
    );
    expect(css).toMatch(/\.dlg\.entry-form-dlg\s*\{[^}]*width:min\(620px,\s*calc\(100vw - 40px\)\)/s);
    expect(css).toMatch(/\.dlg\.access-picker-dlg\s*\{[^}]*width:min\(700px,\s*calc\(100vw - 40px\)\)/s);
  });

  it('节点页只保留一个全局添加菜单，不在组网列表区重复渲染入口或摘要', () => {
    const src = readFileSync(
      fileURLToPath(new URL('../screens/nodes/NodesScreen.tsx', import.meta.url)),
      'utf8',
    );
    expect(src).toContain("t('nodes.meshAddAccess'");
    expect(src).toContain('openMeshJoin();');
    expect(src).not.toContain('!activeGroup?.isMesh');
    expect(src).not.toContain('mesh-list-head');
    expect(src).not.toContain('meshListSummary');
    expect(src).toContain("openDialog({ kind: 'sub', onAdded: setActiveTab })");

    const sub = readFileSync(fileURLToPath(new URL('./SubDialog.tsx', import.meta.url)), 'utf8');
    expect(sub.indexOf('await loadConfig(true);')).toBeLessThan(sub.indexOf('onAdded?.(newSub.id);'));
  });
});
