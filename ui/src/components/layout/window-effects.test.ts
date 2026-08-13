/**
 * `resolveWindowEffectsState` 的口径守卫。
 *
 * 本判定是 Rust `should_apply_window_effects`（src-tauri/src/graphics_compat.rs:65-67）的前端复述，
 * 两侧口径**必须逐字段一致**：一旦漂移，前端会在「Rust 建了不透明窗」时让位（浅色主题透出 #0B0F14
 * 深底 = 不可读），或在「Rust 建了透明窗 + 挂了 vibrancy」时自绘不透明底（= 用户报的「特效未生效」）。
 * 故这里逐条钉的是**语义**（哪种取值组合算开/关），不是实现细节。
 */
import { describe, it, expect } from 'vitest';
import { resolveWindowEffectsState } from './window-effects';

describe('resolveWindowEffectsState：镜像 Rust 两否决位门控', () => {
  it('config 未加载 → unknown（故意不猜，CSS 按不让位兜底）', () => {
    expect(resolveWindowEffectsState(undefined)).toBe('unknown');
  });

  it('两字段皆缺失 → on（与 Rust「缺失即默认开」同口径，存量配置行为不变）', () => {
    expect(resolveWindowEffectsState({})).toBe('on');
  });

  it('两字段皆显式 true → on', () => {
    expect(resolveWindowEffectsState({ windowEffects: true, hardwareAcceleration: true })).toBe(
      'on',
    );
  });

  // 否决位 1：用户直接关特效。
  it('windowEffects=false → off（即便硬件加速开着）', () => {
    expect(resolveWindowEffectsState({ windowEffects: false, hardwareAcceleration: true })).toBe(
      'off',
    );
  });

  // 否决位 2：图形逃生门。Rust 的理由是 vibrancy 本身就是合成层负载，逃生门开着还上特效自相矛盾。
  // 前端若漏掉这一位，就会在逃生门用户（正在自救白屏）的机器上让位给一个根本没挂的特效。
  it('hardwareAcceleration=false → off（即便 windowEffects 开着）', () => {
    expect(resolveWindowEffectsState({ windowEffects: true, hardwareAcceleration: false })).toBe(
      'off',
    );
  });

  it('两位同时 false → off', () => {
    expect(resolveWindowEffectsState({ windowEffects: false, hardwareAcceleration: false })).toBe(
      'off',
    );
  });

  // Rust 用的是 `field_is_explicit_false`：**只有显式 JSON false 才否决**，undefined 不否决。
  // 前端若误写成 `=== true` 之类的真值判定，缺失字段会被判 off —— 存量配置（两字段从未写过）
  // 会集体退回不让位，特效对绝大多数用户静默失效。
  it('单字段缺失不构成否决（另一字段 true）', () => {
    expect(resolveWindowEffectsState({ windowEffects: true })).toBe('on');
    expect(resolveWindowEffectsState({ hardwareAcceleration: true })).toBe('on');
  });
});
