/**
 * WARP 注册腿「先过闸、再打远端」单测（`registerWarpIfSlotFree`）。
 *
 * 守的不变量只有一条，但它是这条腿区别于其它腿的全部理由：**槽位被占 ⇒ 零 Cloudflare 调用**。
 * 其它腿拦在 `server:add` 前，拦晚了只是白建一个本地对象；这条腿的 `registerWarp` 会在 Cloudflare
 * 侧真建一台匿名设备——远端副作用、本地拦不回来、失败面留在对端（孤儿设备 + 可计费）。
 * 故断言的是**调用次数为 0**，不是「返回值为 null」——后者在「先请求、再丢弃结果」的错误实现下同样为真，
 * 是条测不出东西的假绿。
 *
 * 竞态窗口为什么真实存在：接入区卡片只在「打开弹窗那一刻」无 WARP 时才给注册入口，
 * 弹窗停留期间克隆 / 导入 / WgDialog 都能把槽位抢走。
 */
import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { TFunction } from 'i18next';

// `error-handler` 在模块加载期经 `../i18n` 摸 `document`（applyDocumentDirection），而 vitest 跑
// `environment: 'node'` 无 DOM —— 同 `lib/error-handler.test.ts` 的既有做法，只 mock i18n，
// 保留真实 error-handler（`setToastImpl` 注入要用它）。
vi.mock('../i18n', () => ({ default: { t: (k: string) => k } }));

import type { ServerConfig } from '../contracts/types';
import { setToastImpl } from '../lib/error-handler';
import { blockedByMeshSingleton, registerWarpIfSlotFree } from './mesh-singleton-guard';

/** 假 t：有 defaultValue 就用它，否则回显 key（本层只关心「有没有报」，不关心文案本身）。 */
const t = ((key: string, d?: unknown) =>
  typeof d === 'string' ? d : key) as unknown as TFunction;

const wgBase = { privateKey: 'k', localAddress: ['10.0.0.2/32'], peerPublicKey: 'p' };

const srv = (p: Partial<ServerConfig> & { id: string }): ServerConfig =>
  ({ name: p.id, protocol: 'vless', address: '', port: 443, ...p }) as ServerConfig;

/** 已注册的 WARP（带自删凭据，注册腿的真实产物形态）。 */
const warpTagged = (id: string): ServerConfig =>
  srv({
    id,
    protocol: 'wireguard',
    address: '162.159.192.1',
    wireguardSettings: { ...wgBase, warpDevice: { deviceId: 'd', token: 't' } },
  });

/** 旧/导入的 WARP：无 warpDevice，只有端点域名可认。 */
const warpByDomain = (id: string): ServerConfig =>
  srv({
    id,
    protocol: 'wireguard',
    address: 'engage.cloudflareclient.com',
    wireguardSettings: { ...wgBase },
  });

const plainWg = (id: string): ServerConfig =>
  srv({ id, protocol: 'wireguard', address: '203.0.113.7', wireguardSettings: { ...wgBase } });

/** 捕获 toast，避免测试期打 console，同时让「有没有告知用户」可断言。 */
const errors: string[] = [];
beforeEach(() => {
  errors.length = 0;
  setToastImpl({ error: (m) => void errors.push(m) });
});

describe('registerWarpIfSlotFree —— 槽位被占则零远端调用', () => {
  it('**已有带 warpDevice 的 WARP → register 一次都不调**（核心不变量：不烧 CF 匿名设备）', async () => {
    const register = vi.fn(async () => ({ ok: true }));
    const out = await registerWarpIfSlotFree([warpTagged('w1')], t, register);
    expect(register).toHaveBeenCalledTimes(0);
    expect(out).toBeNull();
    expect(errors).toHaveLength(1); // 静默拦截 = 用户点了没反应，必须报
  });

  it('已有「旧 WARP」（无 warpDevice，仅端点域名可认）→ 同样零调用', async () => {
    const register = vi.fn(async () => ({ ok: true }));
    const out = await registerWarpIfSlotFree([warpByDomain('w1')], t, register);
    expect(register).toHaveBeenCalledTimes(0);
    expect(out).toBeNull();
  });

  it('候选判定不依赖用户填的端点：槽位空闲时放行（端点可被改成裸 IP，闸不能挂在域名上）', async () => {
    const register = vi.fn(async () => ({ draft: 1 }));
    const out = await registerWarpIfSlotFree([], t, register);
    expect(register).toHaveBeenCalledTimes(1);
    expect(out).toEqual({ draft: 1 });
    expect(errors).toHaveLength(0);
  });

  it('已有普通 WireGuard / 代理节点不占 WARP 槽 → 正常注册', async () => {
    const register = vi.fn(async () => ({ draft: 2 }));
    const out = await registerWarpIfSlotFree([plainWg('g1'), srv({ id: 'p1' })], t, register);
    expect(register).toHaveBeenCalledTimes(1);
    expect(out).toEqual({ draft: 2 });
  });

  it('已有 Tailscale 不挡 WARP 注册（两个槽互相独立）', async () => {
    const register = vi.fn(async () => ({ draft: 3 }));
    const ts = srv({ id: 't1', protocol: 'tailscale' });
    expect(await registerWarpIfSlotFree([ts], t, register)).toEqual({ draft: 3 });
    expect(register).toHaveBeenCalledTimes(1);
  });

  it('注册失败照常抛出（闸不吞远端错误，否则用户看不到真实失败原因）', async () => {
    const register = vi.fn(async () => {
      throw new Error('cf-429');
    });
    await expect(registerWarpIfSlotFree([], t, register)).rejects.toThrow('cf-429');
    expect(register).toHaveBeenCalledTimes(1);
  });

  it('不 mutate 入参 servers', async () => {
    const servers = [plainWg('g1')];
    await registerWarpIfSlotFree(servers, t, async () => 1);
    expect(servers).toHaveLength(1);
  });
});

describe('blockedByMeshSingleton —— 拦截即报，放行不报', () => {
  it('WARP 槽被占：返回 true 且弹一次错', () => {
    expect(blockedByMeshSingleton(warpTagged('new'), [warpTagged('w1')], t)).toBe(true);
    expect(errors).toHaveLength(1);
  });

  it('Tailscale 槽被占：返回 true 且弹一次错（与 WARP 文案不同，两个槽的自救动作不一样）', () => {
    const ts = (id: string) => srv({ id, protocol: 'tailscale' });
    expect(blockedByMeshSingleton(ts('new'), [ts('t1')], t)).toBe(true);
    expect(errors).toHaveLength(1);
    expect(errors[0]).not.toContain('WARP');
  });

  it('editingId 放行自身：编辑现有 WARP 不报错、不拦', () => {
    expect(blockedByMeshSingleton(warpTagged('w1'), [warpTagged('w1')], t, 'w1')).toBe(false);
    expect(errors).toHaveLength(0);
  });

  it('槽位空闲：返回 false 且不弹错', () => {
    expect(blockedByMeshSingleton(warpTagged('new'), [plainWg('g1')], t)).toBe(false);
    expect(errors).toHaveLength(0);
  });
});
