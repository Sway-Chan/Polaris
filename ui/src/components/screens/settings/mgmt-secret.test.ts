/**
 * 管理面板 API secret 的三颗按钮 + **随机源**守卫（G2）。
 *
 * 立这道门的直接原因：行为对拍表把「重新生成」记成缺口，实际早已实现——
 * 也就是说这条腿**在任何测试的射程之外**，删掉它、或把它改坏，全仓无一处会转红。
 *
 * 真正要钉的是随机源：`clashApiSecret` 是管理 API 的**唯一**鉴权凭据（sing-box `services[0].secret`，
 * 管理 API 能切出口、关连接、读全部连接元数据）。把 `crypto.getRandomValues` 顺手「简化」成
 * `Math.random()` 不会有任何症状——生成的仍是一串十六进制、面板照常连上——而它已经可预测了。
 * 这类改动正是必须当场转红的。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const SRC = code(
  readFileSync(fileURLToPath(new URL('./SettingsNetwork.tsx', import.meta.url)), 'utf8'),
);

describe('管理面板 API secret', () => {
  /**
   * 🔴 随机源必须是 CSPRNG。
   *
   * 变异对照：把 `crypto.getRandomValues(bytes)` 换成 `Math.random()` 填充 → 两条断言同时转红。
   */
  it('secret 由 crypto.getRandomValues 生成，且长度足够', () => {
    expect(SRC).toContain('crypto.getRandomValues(bytes)');
    expect(SRC).toMatch(/new Uint8Array\((\d+)\)/);
    const bytes = Number(/new Uint8Array\((\d+)\)/.exec(SRC)?.[1]);
    // 24 字节 = 192 bit。低于 16 字节（128 bit）对一个长期有效、可被本机任意进程尝试的凭据来说不够。
    expect(bytes).toBeGreaterThanOrEqual(16);
  });

  /**
   * 三颗按钮（显示/隐藏 · 复制 · 重新生成）都接到真实现。
   *
   * 变异对照：删掉重新生成那颗的 `onClick` → 第三条转红。
   */
  it('显示 / 复制 / 重新生成三条腿都在', () => {
    expect(SRC).toContain('setShowSecret((s) => !s)');
    expect(SRC).toContain('copyText(secret)');
    expect(SRC).toContain('update({ clashApiSecret: generateSecret() })');
  });

  /**
   * 重生成走 `update()` 写 `clashApiSecret` —— 该键**在** `UserConfig::FIELD_NAMES` 里
   * （`contracts/user-config-fields.ts`），故 `config_generation_norm` 判不等 → 进重启判定，
   * 与原型 `mgmtRegen()` 的 `markDirty('管理面板 secret 已重置')` 同义。
   *
   * 这条断言防的是「换成一个不在字段集里的键」那类静默失效（即「第四类重启」的造法）。
   */
  it('写的是 UserConfig 字段集里的键（否则改了核不重生成、静默不生效）', async () => {
    const { isCoreConfigKey } = await import('@/contracts/user-config-fields');
    expect(isCoreConfigKey('clashApiSecret')).toBe(true);
  });
});
