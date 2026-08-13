/**
 * 节点「编辑」入口的弹窗路由（纯逻辑，供 NodesScreen 消费 + 单测）。
 *
 * 抽成独立模块而非留在 NodesScreen.tsx 里：这条分流是**数据损坏的防线**，值得被单测钉住
 * （vitest 走 node 环境，不引 jsdom，故纯逻辑必须与组件文件分离——同 csel-logic/wg-logic 先例）。
 */

import type { DialogDesc } from '@/components/dialogs/dialog-store';
import type { ServerConfig } from '@/contracts/types';
import { isWarpServer } from '@/domain/warp';

/**
 * 按协议把编辑请求分流到对应弹窗。
 *
 * 通用 `NodeDialog` 只认 `NodeProto`（13 种代理协议，见 node-spec.ts），**不含 wireguard/tailscale**。
 * 把组网节点丢给它的后果不是「显示不对」而是**丢数据**：`isNodeProto` 判假 → 协议回落 vless →
 * 保存时 `base.protocol !== proto` 让 codecBase 退化成裸 meta → `wireguardSettings` /
 * `tailscaleSettings`（私钥、对端公钥、tailnet 设置）被整体丢弃后覆盖写回。
 *
 * 组网三类各有专用弹窗且 DialogHost 早已注册，此处只是把入口接对。
 */
export function editDialogFor(server: ServerConfig): DialogDesc {
  // WARP 必须先判：它的 protocol 也是 'wireguard'，但走单例槽 warp 弹窗（无 serverId，弹窗自查现有节点）。
  if (isWarpServer(server)) return { kind: 'warp', edit: true };
  if (server.protocol === 'wireguard') return { kind: 'wg', serverId: server.id };
  // Tailscale 是单例：ts-settings 弹窗自查现有节点，不带 serverId。
  if (server.protocol === 'tailscale') return { kind: 'ts-settings' };
  return { kind: 'node', serverId: server.id };
}
