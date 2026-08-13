/**
 * applyFakeIpTunEntry 纯函数单测：消费「FakeIP-TUN 待纠正」快照。
 * 用例集移植自 上游 `src/shared/__tests__/fakeip-tun-entry.test.ts`（穷举触发/不触发面）。
 */
import { describe, it, expect } from 'vitest';
import type { UserConfig } from '@/contracts/types';
import { applyFakeIpTunEntry } from './fakeip-tun-entry';

const cfg = (
  proxyModeType: string,
  enableFakeIp: boolean,
  fakeIpTunAutoEnable?: boolean
): UserConfig =>
  ({
    proxyModeType,
    dnsConfig: { domesticDns: '', foreignDns: '', enableFakeIp, fakeIpTunAutoEnable },
  }) as unknown as UserConfig;

describe('applyFakeIpTunEntry', () => {
  it('tun + 待纠正 + enableFakeIp:false → 回 true 并消费 flag', () => {
    const r = applyFakeIpTunEntry(cfg('tun', false, true));
    expect(r.corrected).toBe(true);
    expect(r.config.dnsConfig?.enableFakeIp).toBe(true);
    expect(r.config.dnsConfig?.fakeIpTunAutoEnable).toBe(false);
  });

  it('tun + 待纠正 + 已开着 → 只消费 flag，corrected=false（不发提示）', () => {
    const r = applyFakeIpTunEntry(cfg('tun', true, true));
    expect(r.corrected).toBe(false);
    expect(r.config.dnsConfig?.enableFakeIp).toBe(true);
    expect(r.config.dnsConfig?.fakeIpTunAutoEnable).toBe(false);
  });

  it('目标模式非 tun（systemProxy）→ 原样返回，flag 存续', () => {
    const r = applyFakeIpTunEntry(cfg('systemProxy', false, true));
    expect(r.corrected).toBe(false);
    expect(r.config.dnsConfig?.enableFakeIp).toBe(false);
    expect(r.config.dnsConfig?.fakeIpTunAutoEnable).toBe(true);
  });

  it('目标模式 manual → 原样返回，flag 存续到进入 tun 才消费', () => {
    const r = applyFakeIpTunEntry(cfg('manual', false, true));
    expect(r.corrected).toBe(false);
    expect(r.config.dnsConfig?.fakeIpTunAutoEnable).toBe(true);
  });

  it('flag=false（已消费/已否决）→ 永不自动改', () => {
    const r = applyFakeIpTunEntry(cfg('tun', false, false));
    expect(r.corrected).toBe(false);
    expect(r.config.dnsConfig?.enableFakeIp).toBe(false);
  });

  it('flag=undefined（未评估）→ 不动', () => {
    const r = applyFakeIpTunEntry(cfg('tun', false, undefined));
    expect(r.corrected).toBe(false);
    expect(r.config.dnsConfig?.enableFakeIp).toBe(false);
  });

  it('不原地改入参（zustand 禁原地改）', () => {
    const input = cfg('tun', false, true);
    const r = applyFakeIpTunEntry(input);
    expect(input.dnsConfig?.enableFakeIp).toBe(false);
    expect(input.dnsConfig?.fakeIpTunAutoEnable).toBe(true);
    expect(r.config).not.toBe(input);
  });

  it('无 dnsConfig → 不炸，原样返回', () => {
    const r = applyFakeIpTunEntry({ proxyModeType: 'tun' } as UserConfig);
    expect(r.corrected).toBe(false);
  });

  it('模式大小写不敏感（TUN）', () => {
    const r = applyFakeIpTunEntry(cfg('TUN', false, true));
    expect(r.corrected).toBe(true);
  });
});
