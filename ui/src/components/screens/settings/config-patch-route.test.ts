/**
 * `splitPatchByRoute` —— 设置页漏斗分流。
 *
 * 变异对照（每条都实跑过：改坏 → 转红 → 从副本还原）：
 *  - 把 `enabled` 透传写死成 `true` → T1 全组红（开关关着却分出了 staged）。
 *  - 把 `direct[key] = value` 漏掉 → T1「direct 与 patch 逐字段相同」红。
 *  - 把 staged/direct 判反（`!== 'staged'`）→ T2 红。
 *  - 用 `Object.assign({}, patch)` 替代逐键分流（= 不分流）→ T2 混合 patch 那条红。
 *  - 把 `staged` 改成按键名排序 → T3 保序那条红。
 */
import { describe, it, expect } from 'vitest';

import { splitPatchByRoute } from './config-patch-route';
import { USER_CONFIG_FIELDS } from '@/contracts/user-config-fields';
import type { UserConfig } from '@/contracts/types';

/** 一份跨两个 class 的代表性 patch：Class B（进投影）与 Class A（纯应用偏好）各若干。 */
const MIXED = {
  mixedPort: 7890, // Class B
  controlPort: 9090, // Class A —— 与上一行同出一个调用点（SettingsNetwork 的 update({[key]:next})）
  allowLan: true, // Class B
  autoStart: false, // Class A
  keepTrayMenuWarm: true, // Class A —— 生命周期偏好应即时落盘，不触发内核暂存/重启
  dnsConfig: { enableFakeIp: true }, // Class B（整对象替换）
  uiTheme: 'dark', // Class A
} as unknown as Partial<UserConfig>;

describe('T1：总开关关着 ⇒ 落盘的那一份与今天逐字段相同（本轮零变化的落点）', () => {
  it('staged 恒空、direct 与入参逐字段相同', () => {
    const r = splitPatchByRoute(MIXED, false);
    expect(r.staged).toEqual([]);
    expect(r.direct).toEqual(MIXED);
  });

  it('**每一个** UserConfig 字段单独成 patch 时也恒走 direct（Class B 一个都不许漏进暂存）', () => {
    for (const k of USER_CONFIG_FIELDS) {
      const r = splitPatchByRoute({ [k]: 1 } as unknown as Partial<UserConfig>, false);
      expect(r.staged, k).toEqual([]);
      expect(r.direct, k).toEqual({ [k]: 1 });
    }
  });

  it('空 patch 两侧都空（不凭空造条目）', () => {
    expect(splitPatchByRoute({}, false)).toEqual({ staged: [], direct: {} });
    expect(splitPatchByRoute({}, true)).toEqual({ staged: [], direct: {} });
  });
});

describe('T2：开关开时按键分流 —— 一个调用点跨两个 class 也各走各的', () => {
  it('Class B 进 staged、Class A 留 direct', () => {
    const { staged, direct } = splitPatchByRoute(MIXED, true);
    expect(staged.map(([k]) => k)).toEqual(['mixedPort', 'allowLan', 'dnsConfig']);
    expect(direct).toEqual({
      controlPort: 9090,
      autoStart: false,
      keepTrayMenuWarm: true,
      uiTheme: 'dark',
    });
  });

  it('两半互斥且并起来等于入参（既不吞键也不重复写）', () => {
    const { staged, direct } = splitPatchByRoute(MIXED, true);
    const union = { ...Object.fromEntries(staged), ...direct };
    expect(union).toEqual(MIXED);
    expect(staged.filter(([k]) => k in direct)).toEqual([]);
  });

  it('W-1/2/3 绕过的键即便是 Class B 也留在 direct（selectedServerId / proxyMode / proxyModeType）', () => {
    const { staged, direct } = splitPatchByRoute(
      { selectedServerId: 'a', proxyMode: 'rule', proxyModeType: 'tun' } as unknown as Partial<UserConfig>,
      true
    );
    expect(staged).toEqual([]);
    expect(Object.keys(direct).sort()).toEqual(['proxyMode', 'proxyModeType', 'selectedServerId']);
  });

  it('值原样透传（不做任何归一化 —— 条目要求的是幂等整体替换）', () => {
    const dns = { enableFakeIp: true, servers: ['1.1.1.1'] };
    const { staged } = splitPatchByRoute({ dnsConfig: dns } as unknown as Partial<UserConfig>, true);
    expect(staged).toHaveLength(1);
    expect(staged[0][1]).toBe(dns);
  });
});

describe('T3：保序（重放结果与条目顺序有关，分流不得重排）', () => {
  it('staged 保持 patch 的键序', () => {
    const { staged } = splitPatchByRoute(
      { tunConfig: 1, allowLan: 2, blockQuic: 3 } as unknown as Partial<UserConfig>,
      true
    );
    expect(staged.map(([k]) => k)).toEqual(['tunConfig', 'allowLan', 'blockQuic']);
  });
});
