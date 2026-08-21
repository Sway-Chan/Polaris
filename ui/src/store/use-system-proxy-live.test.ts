import { describe, expect, it } from 'vitest';
import { isSystemProxyLiveApplicable } from './use-system-proxy-live';

describe('isSystemProxyLiveApplicable', () => {
  it.each([
    { running: false, starting: false, mode: 'systemProxy', expected: false },
    { running: true, starting: true, mode: 'systemProxy', expected: false },
    { running: true, starting: false, mode: 'tun', expected: false },
    { running: true, starting: false, mode: 'manual', expected: false },
    { running: true, starting: false, mode: 'systemProxy', expected: true },
    // 配置尚未水合时沿用既有 systemProxy 兜底，但仍必须是稳定运行态。
    { running: true, starting: false, mode: undefined, expected: true },
  ])('$running/$starting/$mode -> $expected', ({ running, starting, mode, expected }) => {
    expect(isSystemProxyLiveApplicable(running, mode, starting)).toBe(expected);
  });
});
