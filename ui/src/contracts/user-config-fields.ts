/**
 * Rust `UserConfig` 的序列化键集镜像 —— 配置暂存「W-0 豁免谓词」的判据面。
 *
 * # 为什么是这张表，而不是 `config_generation_norm` 的排除表
 *
 * 直觉做法是查 `crates/config-engine/src/builder/orchestration.rs` 的排除表（「改了也不影响生成」的
 * 字段清单）。**那张表答不了这个问题**：`config_generation_norm(config: &UserConfig, …)` 的投影经
 * `serde_json::to_value(config)` 产生，只可能含 Rust `UserConfig` **声明过**的键；排除一个结构里根本
 * 没有的键是空操作。该表 2026-07-29 前有 15 项、其中 14 项（`language` / `hardwareAcceleration` /
 * `subscriptions` …）正是这种死键，已随「对 上游 逐行对拍」判据退役一并删除，现只剩
 * `selectedServerId` 一项。**即便如此它仍不是本表的判据**——它是「真字段里哪些被豁免出判等」，
 * 与「哪些键是真字段」是两个问题。
 *
 * 于是真正的判据只有一条：**这个键在不在 Rust `UserConfig` 里**。
 *
 *  - 不在 ⇒ 结构性不可能进投影 ⇒ 改它恒 `norm_equal` ⇒ 走 NoOp 腿、核零重启 ⇒ 暂存对它**没有意义**
 *    （没有「待应用」可言）⇒ **W-0 豁免，直接落盘**。
 *  - 在 ⇒ 参与投影 ⇒ 改了会走 HotSwitch / Restart 腿 ⇒ **进暂存**。
 *
 * 前端 `UserConfig`（`contracts/types.ts`）是**超集**：它含 `autoStart` / `language` / `windowEffects`
 * 等一大批 Rust 侧没有的应用级偏好键——正是这批键构成 Class A。用前端类型去判豁免会把它们全判错。
 *
 * # 豁免（W-0）≠ 绕过（W-1/2/3）
 *
 * 两者磁盘行为相同（立即落盘），语义不同，**表也必须分开**（见 `lib/staged-config.ts` 的绕过表）：
 * 豁免集会随 Rust `UserConfig` 增字段而自动缩小；绕过集不会。`selectedServerId` 是最典型的分界——
 * 它**在** `UserConfig` 里（故不豁免），但切节点必须立刻生效（故绕过）。
 *
 * # 漂移守门人
 *
 * 本表与 Rust `UserConfig::FIELD_NAMES` 的一致性由 `user-config-fields.test.ts` 双向锁死。
 * 没有那道门，Rust 加一个字段而这里不动 ⇒ 新字段被判「豁免」⇒ 用户改它直接落盘、核静默重启，
 * 而暂存条上从头到尾没提过这件事——这是本谓词**唯一**的失效模式。
 */

/**
 * Rust `UserConfig` 声明的 29 个序列化键，**按声明序**（顺序不参与语义，仅便于与 Rust 侧肉眼对齐）。
 *
 * SoT = `crates/config-engine/src/user_config/app_config.rs` 的 `UserConfig::FIELD_NAMES`。
 */
export const USER_CONFIG_FIELDS = [
  'servers',
  'selectedServerId',
  'proxyMode',
  'proxyModeType',
  'tunConfig',
  'customRules',
  'appRules',
  'appRoutingEnabled',
  'customAppPresets',
  'allowLan',
  'bypassLAN',
  'bypassLANList',
  'enableIPv6',
  'mixedPort',
  'httpPort',
  'dnsConfig',
  'ruleResources',
  'tlsFragment',
  'interruptConnectionsOnSwitch',
  'resolveBeforeDial',
  'regionRouting',
  'fakeIpFilter',
  'fakeIpFilterList',
  'blockBrowserDoh',
  'browserDohList',
  'blockQuic',
  'webrtcLeakProtection',
  'bypassProcesses',
  'clashApiSecret',
  'singboxDashboard',
  // 日志两轴：喂 sing-box `log.*`，改了要重启内核 ⇒ 必须**不豁免**（此前不在 Rust 结构里，
  // 被判豁免 → 直接落盘 → 核继续按旧值跑而暂存条只字不提，即「第四类重启」）。
  'logLevel',
  'disableLogFile',
] as const;

export type UserConfigField = (typeof USER_CONFIG_FIELDS)[number];

const FIELD_SET: ReadonlySet<string> = new Set<string>(USER_CONFIG_FIELDS);

/**
 * 该键是否参与核配置生成（= Rust `UserConfig` 字段）。
 *
 * 入参可以是**点分键路径**（`dnsConfig.enableFakeIp`）：只看首段——子字段的归属由其顶层字段决定，
 * `dnsConfig` 进投影 ⇒ 它的任何子键改动都会让 `norm` 不等。
 */
export function isCoreConfigKey(key: string): boolean {
  const head = key.split('.', 1)[0];
  return FIELD_SET.has(head);
}

/**
 * W-0 豁免谓词：`豁免(key) := key ∉ UserConfigFieldSet`。
 *
 * 豁免 = 该键改动不影响核配置生成 ⇒ 直接落盘，不进暂存。
 */
export function isStagedExempt(key: string): boolean {
  return !isCoreConfigKey(key);
}
