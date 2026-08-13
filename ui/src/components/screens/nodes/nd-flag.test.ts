/**
 * flagCodeForName 单测（vitest，node 环境）——钉住本次修复的根因：旧实现用 `\b` 词边界，
 * 在 "Hk01" 这类数字紧邻字母尾的命名上不成立（K/0 都属 `\w`，两者间无词边界），导致真机
 * 62 个 Hk/Jp 节点全无旗。移植自 上游的 getCountryCode() 用 `(?<![a-z])…(?![a-z])`
 * （左右非小写字母，允许数字/连字符紧邻）替代 `\b`，本测试确认带数字后缀命名正确命中。
 */

import { describe, expect, it } from 'vitest';
import { flagCodeForName } from './nd-flag';

describe('flagCodeForName', () => {
  it('带数字后缀的节点名正确命中（原 \\b 词边界在此漏报的回归用例）', () => {
    expect(flagCodeForName('Hk01')).toBe('hk');
    expect(flagCodeForName('Jp01')).toBe('jp');
    expect(flagCodeForName('Us01')).toBe('us');
    expect(flagCodeForName('HK-02')).toBe('hk');
    expect(flagCodeForName('US_03')).toBe('us');
  });

  it('中文关键词命中', () => {
    expect(flagCodeForName('香港 IEPL 01')).toBe('hk');
    expect(flagCodeForName('东京 02')).toBe('jp');
    expect(flagCodeForName('新加坡节点')).toBe('sg');
  });

  it('无法识别 / 空名 → null（不伪造，静默降级）', () => {
    expect(flagCodeForName('自建中转')).toBeNull();
    expect(flagCodeForName(undefined)).toBeNull();
    expect(flagCodeForName('')).toBeNull();
  });

  it('相邻子串误判防护（flag-detect.ts 边界规则注释用例）：russia 含 us / berlin 含 in 均不误判', () => {
    expect(flagCodeForName('Russia-01')).toBe('ru');
    expect(flagCodeForName('Berlin-02')).toBe('de');
  });
});
