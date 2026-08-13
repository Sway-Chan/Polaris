/**
 * TsLoginDialog 提交前的「节点落盘决策」纯逻辑（与组件分离，走 node 环境单测）。
 *
 * 为何登录前必须先落盘节点：后端把登录产物按 `server.id` 分键存——登录配置写
 * `tailscale-login-<id>.json`（runtime/tailscale_login_core.rs:348），state 目录同样按 id 分
 * （crates/mesh/src/tailscale_login.rs）。拿 `id: ''` 直接发起登录，登录成果会落在空串键下，与日后
 * 任何真实节点 id 永不匹配 = 成果孤儿化；且 config 里从头到尾不会出现 Tailscale 节点。
 *
 * 为何 id 由渲染端 mint 而不是取 `add` 的返回值：`server_add`（commands/server.rs:65）返回
 * `ApiResponse<()>`，**不回传新建节点**（api-client 的 `Promise<ServerConfig>` 签名与后端不符，见
 * 交付说明）；而 `ensure_server_id`（同文件 :40）只在 id 缺失/空串时才 mint，非空 id 原样保留 →
 * 「渲染端自带 id 落盘」是后端明确支持的契约（其单测名即 `..._keeps_existing`）。
 */
import type { ServerConfig } from '@/contracts/types';

export type TsLoginMode = 'browser' | 'authkey';

export interface TsLoginSubmitPlan {
  /** 发给 `tailscaleLogin` 的节点（id 恒为真实 id，绝不为空串）。 */
  server: ServerConfig;
  /** 发起登录前需要的落盘动作。 */
  persist: 'add' | 'update' | 'none';
}

/**
 * 依「有无既有 TS 节点 + 登录方式」算出提交计划。
 *
 * @param mintId 注入而非直接调 `crypto.randomUUID`，使单测可断言 id 去向。
 */
export function planTsLoginSubmit(
  existing: ServerConfig | undefined,
  mode: TsLoginMode,
  authKey: string,
  mintId: () => string
): TsLoginSubmitPlan {
  // 绝不 mutate existing（它是 app-store.servers 里的 live 引用）——克隆基础对象 + 克隆 tailscaleSettings，
  // 提交失败不得把 authKey 写脏内存态。
  const server: ServerConfig = existing
    ? { ...existing, tailscaleSettings: { ...(existing.tailscaleSettings ?? {}) } }
    : ({
        id: mintId(),
        name: 'Tailscale',
        protocol: 'tailscale',
        // tailscale 是账号制协议，后端 sanitize 豁免 address/port 校验（crates/store/src/sanitize.rs:250）。
        address: '',
        port: 0,
        // 空设置即默认：allowInternet / alwaysRouteSubnets 缺省均为 true（contracts/types/protocol-settings.ts:179,182），
        // 显式重写一遍只会制造第二个默认值真值源。
        tailscaleSettings: {},
      } as ServerConfig);

  if (mode === 'authkey') {
    server.tailscaleSettings = {
      ...(server.tailscaleSettings ?? {}),
      authKey: authKey.trim(),
    };
  }

  if (!existing) return { server, persist: 'add' };
  // 已有节点：只有 authKey 真的变了才写盘。browser 模式不碰配置 → 免掉一次无谓的
  // CONFIG_CHANGED 广播（后端据此重启代理）。
  const keyChanged =
    mode === 'authkey' &&
    server.tailscaleSettings?.authKey !== existing.tailscaleSettings?.authKey;
  return { server, persist: keyChanged ? 'update' : 'none' };
}
