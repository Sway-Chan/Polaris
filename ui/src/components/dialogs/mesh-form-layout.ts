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

export type TsFormTab = 'basic' | 'routing' | 'advanced';

export const TS_FORM_GROUP_KEYS: Record<TsFormTab, readonly string[]> = {
  basic: ['hostname', 'exitNode', 'exitNodeCustom'],
  routing: [
    'reverseMesh',
    'alwaysRouteSubnets',
    'acceptRoutes',
    'routes',
    'exitNodeAllowLanAccess',
    'advertiseRoutes',
  ],
  advanced: [
    'detour',
    'controlUrl',
    'advertiseTags',
    'ephemeral',
    'relayServerPort',
    'sshServer',
    'resolveByName',
    'acceptDefaultResolvers',
  ],
};

/** Tailscale 字段按任务分组；FieldSpec 与保存逻辑仍各自只有一份真值。 */
export function groupTsFields(fields: FieldSpec[]): Record<TsFormTab, FieldSpec[]> {
  const inGroup = (tab: TsFormTab, key: string) => TS_FORM_GROUP_KEYS[tab].includes(key);
  return {
    basic: fields.filter((field) => inGroup('basic', field.k)),
    routing: fields.filter((field) => inGroup('routing', field.k)),
    advanced: fields.filter((field) => inGroup('advanced', field.k)),
  };
}

export type WarpFormTab = 'basic' | 'routing' | 'advanced';

export const WARP_FORM_GROUP_KEYS: Record<WarpFormTab, readonly string[]> = {
  basic: ['endpoint'],
  routing: ['route', 'allowedIPs', 'reverseMesh'],
  advanced: ['mtu', 'keepalive', 'reserved', 'detour'],
};

/** WARP 的 WireGuard 字段按任务分组；名称与许可证作为手写基础字段留在同一页签。 */
export function groupWarpFields(fields: FieldSpec[]): Record<WarpFormTab, FieldSpec[]> {
  const inGroup = (tab: WarpFormTab, key: string) => WARP_FORM_GROUP_KEYS[tab].includes(key);
  return {
    basic: fields.filter((field) => inGroup('basic', field.k)),
    routing: fields.filter((field) => inGroup('routing', field.k)),
    advanced: fields.filter((field) => inGroup('advanced', field.k)),
  };
}

/** OpenConnect / OpenVPN 组网隧道的本地语法门；内核语义仍由后端最终校验。 */
export function meshTunnelDraftError(
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
