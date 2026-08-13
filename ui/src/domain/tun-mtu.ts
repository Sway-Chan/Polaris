/**
 * TUN 栈解析 + 默认 MTU 派生 —— 与 Rust 侧 `crates/config-engine/src/user_config/tun_stack.rs`
 * 逐值同口径的渲染端副本。
 *
 * # 为什么渲染端需要一份
 *
 * MTU 设置项的「自动」态要**当场告诉用户自动是多少**（占位符里的那个数）。不算出来就只能写死一句
 * 「按平台自动」——那等于让用户去猜，而这一项的取值范围跨了 16 倍（4064 ↔ 65535），猜不出来。
 *
 * # 两侧同口径怎么保证
 *
 * 生成期的真值**始终在 Rust**（本文件只影响显示，改坏了也不会让内核拿到别的 MTU）。
 * 同口径由 `tun-mtu.test.ts` 的表锁住：那张表与 Rust 的
 * `default_mtu_by_stack_and_platform` 用例逐格对应，任一侧改值而另一侧没跟，两边各红一处。
 *
 * 数值判据（实测，非推理）见 Rust 侧同名函数的文档注释与
 * vault `design/networking/` 下的 win-tun MTU 基准记录。
 */

import type { TunStack } from '@/contracts/types';

/** 下发给核的具体栈（永不含 auto）。 */
export type ConcreteTunStack = 'system' | 'gvisor' | 'mixed';

/** 渲染端平台标识 —— 与 `settings-logic.ts::shellPlatformFromDataOs()` 的返回值域一致
 * （含 `undefined`：SSR / 测试渲染下 `document` 不存在，取不到平台）。此处**重声明而非 import**：
 * `domain/` 是纯逻辑层，不反向依赖 `components/`。两者对不上时 `SettingsTun` 的调用处编译报错。 */
export type ShellPlatform = 'mac' | 'win' | 'lin' | undefined;

/**
 * 平台默认栈：mac·win→gvisor / linux→system / 未知→system。
 *
 * Windows 2026-08-05 从 system 改到 gvisor：同机实测 `gvisor/65535` = 919 Mbps 而
 * `system/65535` = 11 Mbps（链路层塌陷），且 gvisor 下 MTU 越大越快。
 * Linux 保持 system —— 实测栈间无可测差异，没有换默认的依据。
 */
export function platformDefaultStack(platform: ShellPlatform): ConcreteTunStack {
  return platform === 'mac' || platform === 'win' ? 'gvisor' : 'system';
}

/** auto/缺省 → 平台默认；显式 system/gvisor/mixed → 原样（全平台 honor）。 */
export function resolveTunStack(
  userStack: TunStack | undefined,
  platform: ShellPlatform
): ConcreteTunStack {
  return userStack === undefined || userStack === 'auto'
    ? platformDefaultStack(platform)
    : userStack;
}

/**
 * 未显式设置 MTU 时的默认值，按**最终栈 × 平台**取。
 *
 * - gvisor + Windows → 65535（实测全矩阵最高格）
 * - gvisor + 其余 → 9000（mac/linux 的吞吐实验都没跑出区分力 → 不照搬 Windows 的极端值）
 * - system / mixed → 4064（下界：1350 会丢 1400B UDP，三平台各自复现；上界：65535 塌到 11 Mbps）
 */
export function defaultMtuFor(stack: ConcreteTunStack, platform: ShellPlatform): number {
  if (stack === 'gvisor') return platform === 'win' ? 65535 : 9000;
  return 4064;
}

/** 当前配置下「自动」会取到的 MTU（供设置项占位符显示）。 */
export function autoMtuFor(userStack: TunStack | undefined, platform: ShellPlatform): number {
  return defaultMtuFor(resolveTunStack(userStack, platform), platform);
}

/** 内核接受的 MTU 区间（与 `polaris-store` 的 `validate_config` 同值）。 */
export const MTU_MIN = 1280;
export const MTU_MAX = 65535;

/**
 * 输入框文本 → 待持久化的 MTU。
 *
 * - 空白 → `{ mtu: undefined }`（自动）
 * - 合法整数且在区间内 → `{ mtu: n }`
 * - 其余（非数字 / 越界 / 小数）→ `{ invalid: true }`，调用方**不落库**并标红
 *
 * 越界不做钳制：把 70000 悄悄改成 65535 与「设了但不生效」是同一类缺陷，用户看到的框里是自己填的
 * 数，生效的是另一个。宁可当场报错。
 */
export function parseMtuInput(text: string): { mtu?: number; invalid?: true } {
  const s = text.trim();
  if (s === '') return { mtu: undefined };
  if (!/^\d+$/.test(s)) return { invalid: true };
  const n = Number(s);
  if (!Number.isSafeInteger(n) || n < MTU_MIN || n > MTU_MAX) return { invalid: true };
  return { mtu: n };
}
