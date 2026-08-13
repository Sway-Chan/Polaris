import { describe, expect, it } from 'vitest';
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
});
