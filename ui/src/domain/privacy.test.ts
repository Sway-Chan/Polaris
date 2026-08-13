import { describe, it, expect } from 'vitest';
import {
  IDLE_PRIVACY_LOCK_MS,
  resolveUnlockAttempt,
  shouldArmIdleLock,
  shouldRedactLogs,
  redactSensitive,
} from './privacy';

describe('resolveUnlockAttempt（解锁提交决策）', () => {
  it('已设密码 + 空输入 → require-input（本地提示，不打后端）', () => {
    expect(resolveUnlockAttempt(true, '')).toBe('require-input');
  });
  it('已设密码 + 非空 → unlock（交后端校验）', () => {
    expect(resolveUnlockAttempt(true, 'hunter2')).toBe('unlock');
  });
  it('未设密码 + 空输入 → unlock（后端空 hash 自由解锁；空密码放行）', () => {
    expect(resolveUnlockAttempt(false, '')).toBe('unlock');
  });
  it('未设密码 + 非空 → unlock（后端忽略输入，恒 ok）', () => {
    expect(resolveUnlockAttempt(false, 'whatever')).toBe('unlock');
  });
});

describe('shouldArmIdleLock（闲置计时武装）', () => {
  it('开关开 + 未锁 → 武装', () => {
    expect(shouldArmIdleLock(true, false)).toBe(true);
  });
  it('开关开 + 已锁 → 不武装（避免重复触发）', () => {
    expect(shouldArmIdleLock(true, true)).toBe(false);
  });
  it('开关关 → 恒不武装', () => {
    expect(shouldArmIdleLock(false, false)).toBe(false);
    expect(shouldArmIdleLock(false, true)).toBe(false);
  });
});

describe('IDLE_PRIVACY_LOCK_MS', () => {
  it('= 10 分钟（原型「闲置 10 分钟」）', () => {
    expect(IDLE_PRIVACY_LOCK_MS).toBe(10 * 60 * 1000);
  });
});

describe('shouldRedactLogs（C18 实时日志脱敏门）', () => {
  it('隐私锁开 → 恒脱敏（无论偏好）', () => {
    expect(shouldRedactLogs(true, false)).toBe(true);
    expect(shouldRedactLogs(true, true)).toBe(true);
  });
  it('常态脱敏偏好开 → 脱敏（即便未锁）', () => {
    expect(shouldRedactLogs(false, true)).toBe(true);
  });
  it('未锁 + 偏好关 → 不脱敏（默认，保调试可见性）', () => {
    expect(shouldRedactLogs(false, false)).toBe(false);
  });
});

describe('redactSensitive（日志脱敏）', () => {
  it('掩去 IPv4', () => {
    expect(redactSensitive('management api 127.0.0.1:9090')).toBe('management api •••:9090');
    expect(redactSensitive('egress 203.0.113.42')).toBe('egress •••');
  });
  it('掩去域名（含子域）', () => {
    expect(redactSensitive('connection opened → example.com')).toBe('connection opened → •••');
    expect(redactSensitive('dns resolve api.sing-box.io')).toBe('dns resolve •••');
  });
  it('同一行域名 + IP 都掩', () => {
    expect(redactSensitive('proxy example.com via 10.0.0.1')).toBe('proxy ••• via •••');
  });
  it('掩去 IPv6（≥3 段冒号）', () => {
    expect(redactSensitive('peer 2001:0db8:85a3:0000:0000:8a2e:0370:7334 up')).toBe('peer ••• up');
  });
  it('不误伤时间戳（HH:MM:SS 仅 2 冒号）', () => {
    expect(redactSensitive('started at 02:53:39 ok')).toBe('started at 02:53:39 ok');
  });
  it('不误伤版本号 / 无点单词', () => {
    expect(redactSensitive('sing-box 1.14.0-alpha.43 ready')).toBe('sing-box 1.14.0-alpha.43 ready');
    expect(redactSensitive('hot-switch selector → 东京 03')).toBe('hot-switch selector → 东京 03');
    expect(redactSensitive('helper connected v3')).toBe('helper connected v3');
  });
});
