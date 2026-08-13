/**
 * `UserConfigFieldSet` 的**跨语言双向锁** —— 前端 `USER_CONFIG_FIELDS` ↔ Rust `UserConfig::FIELD_NAMES`。
 *
 * # 这道门守的是什么
 *
 * 暂存的 W-0 豁免谓词是一行：`豁免(key) := key ∉ UserConfigFieldSet`。它唯一的失效模式是**两侧漂移**：
 *
 *  - **只改 Rust**（给 `UserConfig` 加字段、前端表不动）→ 新字段被判「豁免」→ 用户改它直接落盘、
 *    `config_generation_norm` 判不等 → 核静默重启，而暂存条从头到尾没提过它。
 *  - **只改前端**（手抄漏一项 / 拼错）→ 该字段被判「豁免」，同上；或凭空多一项 → 一个不存在的键被判
 *    「进暂存」，用户的编辑卡在暂存里永远等不到它该等的东西。
 *
 * 两个方向都**不会**被类型检查、build、其它任何测试发现——豁免判据是运行期字符串比对，判错了照样全绿。
 *
 * # 为什么读 Rust 源码而不是再抄一份镜像常量
 *
 * 抄镜像只是把漂移面往后挪一格。范式照抄本仓既有的 `unlock-detection.test.ts`（锁 `ServiceId::ALL`）：
 * 直接把 Rust 源码当真值读进来解析，任一侧单独改动都会转红。
 *
 * # Rust 侧还有第二道门（本门够不着的那半）
 *
 * 本门锁的是「常量表 ↔ 前端表」。「常量表 ↔ 结构体真实字段」由 Rust 侧
 * `app_config.rs` 的 `field_names_equals_serde_projection` + 穷尽结构字面量（新增字段即 E0063 编译失败）
 * 锁死。两道门串起来才构成「Rust 结构体 → 前端豁免表」的完整链条。
 *
 * # 自曝纪律
 *
 * 解析器解析不到必须转红，而不是拿到空数组让后面的断言恒真——「没检查」与「检查通过」的输出不可区分
 * = 没有这道门。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { USER_CONFIG_FIELDS, isCoreConfigKey, isStagedExempt } from './user-config-fields';

/** Rust 侧 SoT。 */
const RUST_APP_CONFIG = readFileSync(
  fileURLToPath(new URL('../../../crates/config-engine/src/user_config/app_config.rs', import.meta.url)),
  'utf8'
);

/** 取 `pub const FIELD_NAMES: &'static [&'static str] = &[ … ];` 里的字面量（保序）。 */
function rustFieldNames(src: string): string[] {
  const block = /pub const FIELD_NAMES: &'static \[&'static str\] = &\[([\s\S]*?)\];/.exec(src);
  expect(
    block,
    'Rust 侧 UserConfig::FIELD_NAMES 解析失败（改名/重构了？）—— 解析不到必须转红，不得静默放行'
  ).not.toBeNull();
  return [...block![1].matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

const RUST_FIELDS = rustFieldNames(RUST_APP_CONFIG);

describe('解析器自检（没解析到必须自曝）', () => {
  it('Rust 侧字段表非空且规模合理', () => {
    // 下界取 20：`UserConfig` 是「增量子集」，只会随移植推进增长，缩到 20 以下只可能是解析错了。
    expect(RUST_FIELDS.length).toBeGreaterThanOrEqual(20);
  });

  it('前端表无重复项（重复会让集合断言在「漏一项 + 抄重一项」时假绿）', () => {
    expect(new Set<string>(USER_CONFIG_FIELDS).size).toBe(USER_CONFIG_FIELDS.length);
  });
});

describe('前端 USER_CONFIG_FIELDS ↔ Rust UserConfig::FIELD_NAMES 双向锁', () => {
  /**
   * 逐一同序相等。
   *
   * 牙（两个方向各一）：
   *  · 只改 Rust（给 `UserConfig` 加 `fooBar` 并补进 `FIELD_NAMES`）→ Rust 多一项 → 转红。
   *  · 只改前端（从 `USER_CONFIG_FIELDS` 删 `singboxDashboard`）→ 前端少一项 → 转红。
   */
  it('两侧字段表逐项同序相等', () => {
    expect([...USER_CONFIG_FIELDS]).toEqual(RUST_FIELDS);
  });
});

describe('W-0 豁免谓词', () => {
  /**
   * E-4：`autoStart` 那一批（10 个应用级偏好键）在**前端** `UserConfig` 里真实存在、也在 Rust
   * `sanitize.rs` 的合法键表里，但**不在** Rust `UserConfig` 里 ⇒ Class A 豁免。
   *
   * 牙：把判据改回「查 `config_generation_norm` 的排除表」→ 这 10 个键一个都不在那张表里
   * → 全被判「进暂存」→ 转红。这正是必须用「是否 `UserConfig` 字段」而非查表的原因。
   * （该表 2026-07-29 已从 15 项缩到 1 项，查表判据比当年更不可用，本条牙只会更硬。）
   */
  it('Class A：应用级偏好键豁免（E-4 那一批）', () => {
    for (const key of [
      'autoStart',
      'silentStart',
      'autoConnect',
      'minimizeToTray',
      'autoCheckUpdate',
      'autoLightweightMode',
      'rememberWindowSize',
      'desktopNotifications',
      'autoUpdateSubscriptionOnStart',
      'autoPrivacyMode',
    ]) {
      expect(isStagedExempt(key), `${key} 应为 Class A 豁免`).toBe(true);
    }
  });

  /**
   * E-6：`hardwareAcceleration` / `windowEffects` 是 Class A —— 它们需要重启 **App**，与核无关，
   * 不进 pending 差集（重启提示由 `domain/app-restart-keys.ts` 那条腿负责）。
   */
  it('Class A：需重启 App 的键与 language 豁免（E-6）', () => {
    for (const key of ['hardwareAcceleration', 'windowEffects', 'language']) {
      expect(isStagedExempt(key), `${key} 应为 Class A 豁免`).toBe(true);
    }
  });

  /**
   * E-5：`singboxDashboard` 名字像 UI 偏好（开个面板），但它**是** `UserConfig` 字段
   * （注入 `services[0].dashboard`）⇒ Class B 进暂存。
   * E-8：设置页主体（`dnsConfig` / `tunConfig` / `regionRouting`）同为 Class B——
   * 「设置页」不是分类维度，同一页里 DNS 开关与桌面通知归属不同。
   *
   * **E-3 已于 2026-07-29 改判**：`logLevel` / `disableLogFile` 一直确实影响生成（经
   * `GenerateConfigDeps` 注入 sing-box `log.*`），当时判「豁免」的理由是「它们不在 Rust `UserConfig`
   * ⇒ norm 看不见 ⇒ 核零重启」—— 那不是设计，是缺陷（「第四类重启」：改了要重启核，而 pending 差集
   * 与 U-7 弹窗都不出现）。两键现已建模进 `UserConfig`，norm 看得见了 ⇒ 归 Class B。
   */
  it('Class B：影响生成的键不豁免（E-5 / E-8 / 改判后的 E-3）', () => {
    for (const key of [
      'singboxDashboard',
      'dnsConfig',
      'tunConfig',
      'regionRouting',
      'servers',
      'customRules',
      'logLevel',
      'disableLogFile',
    ]) {
      expect(isStagedExempt(key), `${key} 应为 Class B 进暂存`).toBe(false);
    }
  });

  /**
   * 点分键路径按**首段**归属：`dnsConfig` 进投影 ⇒ 它任何子键的改动都会让 norm 不等。
   *
   * 牙：把 `isCoreConfigKey` 改成整串比对 → `dnsConfig.enableFakeIp` 查不到 → 被判豁免 → 转红。
   */
  it('点分键路径按首段判定', () => {
    expect(isCoreConfigKey('dnsConfig.enableFakeIp')).toBe(true);
    expect(isStagedExempt('dnsConfig.enableFakeIp')).toBe(false);
    expect(isStagedExempt('helperPromptDismissed.foo')).toBe(true);
  });

  /**
   * E-1：`subscriptions` 曾在 `config_generation_norm` 的排除表里（直觉「豁免」），而它**不是**
   * `UserConfig` 字段 ⇒ 那个表项本身是死的，2026-07-29 已随其余 13 个死键一并删除。
   * 结论不变：订阅**元数据**确属 Class A 豁免，且判据自始至终是「不在 `UserConfig` 里」而非查表。
   * （订阅编辑导致的 `servers[]` 变化属 Class B；手动刷新订阅另按 W-3 绕过。）
   */
  it('E-1：subscriptions 元数据属 Class A（判据是 UserConfig 字段集，不是排除表）', () => {
    expect(isStagedExempt('subscriptions')).toBe(true);
    expect(isStagedExempt('servers')).toBe(false);
  });
});
