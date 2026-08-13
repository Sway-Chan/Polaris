import type { FieldSpec, FormValues } from './field-spec';
import type { NodeProto } from './node-spec';

export type WgFormTab = 'basic' | 'routing' | 'advanced';

export const WG_FORM_GROUP_KEYS: Record<WgFormTab, readonly string[]> = {
  basic: ['address', 'port', 'privateKey', 'localAddress', 'peerPublicKey', 'preSharedKey'],
  routing: ['allowedIPs', 'reverseMesh', 'allowInternet', 'alwaysRouteSubnets'],
  advanced: ['persistentKeepalive', 'mtu', 'reserved', 'detour'],
};

/** WireGuard 字段按用户任务分组；字段定义本身仍只有调用方的一份。 */
export function groupWgFields(fields: FieldSpec[]): Record<WgFormTab, FieldSpec[]> {
  const inGroup = (tab: WgFormTab, key: string) => WG_FORM_GROUP_KEYS[tab].includes(key);
  return {
    basic: fields.filter((field) => inGroup('basic', field.k)),
    routing: fields.filter((field) => inGroup('routing', field.k)),
    advanced: fields.filter((field) => inGroup('advanced', field.k)),
  };
}

/** 长 VPN 表单的本地语法门；内核语义仍由后端最终校验。 */
export function vpnDraftError(
  proto: NodeProto,
  draft: FormValues
): { tab: 'basic' | 'routing' | 'advanced'; key: 'required' | 'json' } | null {
  const present = (key: string) => typeof draft[key] === 'string' && draft[key].trim() !== '';
  if (proto === 'openconnect' && (!present('user') || !present('pwd') || !present('flavor'))) {
    return { tab: 'basic', key: 'required' };
  }
  if (proto === 'openvpn-client' && (!present('user') || !present('pwd') || !present('ovpnCa'))) {
    return { tab: 'basic', key: 'required' };
  }
  for (const key of ['extraJson', 'ovpnTlsExtraJson']) {
    const raw = draft[key];
    if (typeof raw !== 'string' || raw.trim() === '') continue;
    try {
      const parsed: unknown = JSON.parse(raw);
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
        return { tab: 'advanced', key: 'json' };
      }
    } catch {
      return { tab: 'advanced', key: 'json' };
    }
  }
  return null;
}
