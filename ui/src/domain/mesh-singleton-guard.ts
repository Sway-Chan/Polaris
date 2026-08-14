/**
 * 组网单例槽（WARP / Tailscale）提交闸门的**渲染侧收口**：判定（纯）+ 拒绝文案 + toast。
 *
 * 判定真值在 `endpoint-routes.ts#meshSingletonConflict`；本文件只做「i18n 文案 + toast」这层，
 * 与 `subscription-refresh.ts` 同型（domain 里持 `t` 并弹 toast 的既有先例），使 NodeDialog /
 * WgDialog / ImportDialog 三条造节点腿的拒绝语义**一处定义**，不各写一份文案。
 *
 * 为什么闸门必须落在「提交前」而不是接入区卡片上：接入区只控入口显隐，粘贴 Cloudflare `.conf`
 * 的 WgDialog、批量入库的 ImportDialog、手输的 NodeDialog 都能绕开接入区直调 `server:add`。
 */
import type { TFunction } from 'i18next';
import { toast } from '@/lib/error-handler';
import {
  meshSingletonConflict,
  type MeshSingletonSlot,
  type MeshSlotServer,
} from './endpoint-routes';

/** 单例槽被占的拒绝文案（两个槽各一句，说明「为什么不能有第二个」+「怎么办」）。 */
export function meshSingletonMessage(slot: MeshSingletonSlot, t: TFunction): string {
  return slot === 'warp'
    ? t('nodes.warpSlotTaken')
    : t('nodes.tsSlotTaken');
}

/**
 * 提交前闸门：候选撞上已占的单例槽即弹错并返回 true（调用方据此中止提交）。
 * `editingId` 传当前编辑对象的 id——编辑现有 WARP/TS 节点不算「再加一个」，必须放行。
 */
export function blockedByMeshSingleton(
  candidate: MeshSlotServer,
  servers: MeshSlotServer[],
  t: TFunction,
  editingId?: string
): boolean {
  const slot = meshSingletonConflict(candidate, servers, editingId);
  if (!slot) return false;
  toast.error(meshSingletonMessage(slot, t));
  return true;
}

/**
 * WARP 注册**将要**产出的节点形态。注册产物恒带自删凭据 `warpDevice` ⇒ 恒被 `isWarpServer` 判为 WARP，
 * 故这个常量就是「这次注册会造出什么」的忠实描述。
 *
 * 为什么不按用户填的端点 host 判：端点字段可编辑（`WarpDialog` 默认 `engage.cloudflareclient.com:2408`，
 * 但用户可改成 `162.159.192.1:2408` 之类的裸 IP）。靠域名兜底判定会在用户改过端点时**漏判**，
 * 而 `warpDevice` 这条腿对任何端点都成立。真凭据要等 Cloudflare 应答才有，闸却必须在应答之前 →
 * 用「将要产出的形态」判，是这条腿唯一可能的判据。
 */
const WARP_REGISTRATION_CANDIDATE: MeshSlotServer = {
  protocol: 'wireguard',
  wireguardSettings: { warpDevice: true },
};

/**
 * WARP 注册腿的**先过闸、再打远端**：槽位被占则一次 Cloudflare 请求都不发，返回 null。
 *
 * 为什么闸必须前置到请求之前（而不是像其它腿那样拦在 `server:add` 前）：`registerWarp` 会在
 * Cloudflare 侧**真建一台匿名设备**——远端副作用，本地拦截无法回滚，失败面留在对端（孤儿设备 +
 * 可计费）。接入区卡片确实只在「无 WARP 节点」时才给注册入口，但那是**打开弹窗那一刻**的快照：
 * 弹窗停留期间由克隆 / 导入 / WgDialog / 另一处入口造出 WARP，提交时槽位已被抢——先打请求再被拦
 * = 白烧一台设备。故这里是竞态窗口的收口，不是入口闸的重复。
 *
 * 不收 `editingId`：本函数只服务注册（新增）腿，语义恒为「再加一个」，与克隆同——现有 WARP 自身即占槽。
 * 编辑现有 WARP 走 `applyWarpLicense` / `server:update`，不经本函数。
 *
 * `register` 注入而非直接调 `api`，使「槽位被占 ⇒ 零远端调用」这条不变量可离线单测（内联闭包测不到）。
 */
export async function registerWarpIfSlotFree<T>(
  servers: MeshSlotServer[],
  t: TFunction,
  register: () => Promise<T>
): Promise<T | null> {
  if (blockedByMeshSingleton(WARP_REGISTRATION_CANDIDATE, servers, t)) return null;
  return register();
}
