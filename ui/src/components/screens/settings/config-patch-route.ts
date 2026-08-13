/**
 * 设置页 patch 的分流 —— `useConfig().update` 漏斗的纯逻辑那一半。
 *
 * # 为什么分流在漏斗里、按键判，而不是在 46 个调用点上判
 *
 * `~/docs/polaris/design/polaris-userconfig-write-entrypoints-2026-07-29.md` 举的反例就在设置页：
 * `SettingsNetwork.tsx` 的 `update({ [key]: next })` 一个调用点跨 `mixedPort`(Class B) 与
 * `controlPort`(Class A) 两个 class —— 键在**运行期**才知道，静态判必错一半。
 * 9 个子页共用同一个 `update` 函数引用，故「按键判」只需在这里做一次。
 *
 * # 为什么返回两半而不是一个布尔
 *
 * 一次 patch 可以同时携带两个 class 的键（`SettingsTun` 一次提交改 `tunConfig` + `dnsConfig`；
 * 未来任何一个「一个开关改两处」的入口同理）。判成一个整体就必然把其中一半送错腿：
 * 送错到 direct = 该键静默绕过暂存；送错到 staged = Class A 键被压进一条永远没人应用的条目。
 */

import { editRoute } from '@/lib/staged-config';
import type { UserConfig } from '@/contracts/types';

export interface PatchRoute {
  /** 该进暂存的键值对，**保持 patch 的键序**（重放对顺序敏感，别在这里重排）。 */
  readonly staged: ReadonlyArray<readonly [string, unknown]>;
  /** 仍旧直接落盘的那一半。`enabled=false` 时它与入参逐字段相同。 */
  readonly direct: Partial<UserConfig>;
}

/**
 * 按 `editRoute` 把 patch 拆成「进暂存」与「直接落盘」两半。
 *
 * **`enabled=false` ⇒ `staged` 恒空、`direct` 与 `patch` 逐字段相同**（`editRoute` 此时恒返 `'direct'`）。
 * 这是「总开关关着时行为零变化」在漏斗这一侧的落点，由 `config-patch-route.test.ts` 钉住。
 */
export function splitPatchByRoute(patch: Partial<UserConfig>, enabled: boolean): PatchRoute {
  const staged: Array<readonly [string, unknown]> = [];
  const direct: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(patch)) {
    if (editRoute(key, enabled) === 'staged') staged.push([key, value]);
    else direct[key] = value;
  }
  return { staged, direct: direct as Partial<UserConfig> };
}
