/**
 * 连接态按接管方式分叉（契约 L17）的档位矩阵。
 *
 * 守的是「systemProxy 下 `running` 不等于连上了」这条语义 —— 它正是审计里
 * 「绿灯 + 明文直连」误导的根因；退回 running-only 会让 `systemProxy + SYSTEM_PROXY_FAILED`
 * 那一格转红。
 */
import { describe, it, expect } from 'vitest';
import { ProxyErrorCode } from '@/contracts/types';
import { deriveTakeoverConnState, isTrulyConnected } from './connection-state';

describe('deriveTakeoverConnState', () => {
  it('核未运行 → disconnected（三种接管方式一致）', () => {
    for (const mode of ['systemProxy', 'tun', 'manual'] as const) {
      expect(
        deriveTakeoverConnState({ running: false, proxyModeType: mode, errorCode: undefined })
      ).toBe('disconnected');
    }
  });

  it('TUN/manual 只看 running —— 即便带着 SYSTEM_PROXY_FAILED 也算已连接', () => {
    for (const mode of ['tun', 'manual'] as const) {
      expect(
        deriveTakeoverConnState({
          running: true,
          proxyModeType: mode,
          errorCode: ProxyErrorCode.SYSTEM_PROXY_FAILED,
        })
      ).toBe('connected');
    }
  });

  it('systemProxy + 系统代理启用失败 → proxy-degraded（不得报「已连接」）', () => {
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: ProxyErrorCode.SYSTEM_PROXY_FAILED,
      })
    ).toBe('proxy-degraded');
  });

  it('systemProxy 且无该码 → connected（其它错误码不冒充系统代理失效）', () => {
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: undefined,
      })
    ).toBe('connected');
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: ProxyErrorCode.RULE_RESOURCES_MISSING,
      })
    ).toBe('connected');
  });

  it('config 未水合（proxyModeType undefined）→ 按 systemProxy 兜底，但不凭空造降级态', () => {
    expect(
      deriveTakeoverConnState({ running: true, proxyModeType: undefined, errorCode: undefined })
    ).toBe('connected');
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: undefined,
        errorCode: ProxyErrorCode.SYSTEM_PROXY_FAILED,
      })
    ).toBe('proxy-degraded');
  });

  // ── 活态分支（`system_proxy_get_status` 的 pointsToUs）────────────────────────────────
  //
  // 这一组锁的是活态查询存在的**理由**：起核那一刻的 errorCode 测不出的两种形态，
  // 以及「读不到 ≠ 没生效」的方向性。

  it('运行期用户手动关掉 OS 代理 → 活态 not-effective 即判降级（errorCode 干净也拦得住）', () => {
    // 这正是活态查询要补的第一条漏报腿：起核成功 → errorCode 恒 undefined，
    // 只看 errorCode 会一路绿灯 + 明文直连。
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: undefined,
        systemProxyLive: 'not-effective',
      })
    ).toBe('proxy-degraded');
  });

  it('SYSTEM_PROXY_FAILED 被后来的非终态错误覆盖后，活态仍能判出降级（单槽腿）', () => {
    // 第二条漏报腿：error_code 是单槽，RULE_RESOURCES_MISSING 会把 SYSTEM_PROXY_FAILED 挤掉。
    // 活态不读那个槽 → 不受覆盖影响。
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: ProxyErrorCode.RULE_RESOURCES_MISSING,
        systemProxyLive: 'not-effective',
      })
    ).toBe('proxy-degraded');
  });

  it('活态 effective 是权威：即便带着起核期的 SYSTEM_PROXY_FAILED 也判已连接', () => {
    // 起核那一刻失败、随后用户手动补设 / 重试成功 —— 此刻 OS 代理确实指向我们。
    // 继续挂降级 = 拿陈旧记录压地面真相。
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: ProxyErrorCode.SYSTEM_PROXY_FAILED,
        systemProxyLive: 'effective',
      })
    ).toBe('connected');
  });

  it('活态 unknown（读不到/未取到）→ 回落 errorCode 腿，不凭空造降级态', () => {
    // 「读不到 ≠ 没生效」：非 GNOME 桌面 / PATH 缺 reg.exe / 首帧未到 都会是 unknown，
    // 折成降级会让这些环境稳定误亮黄灯。
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: undefined,
        systemProxyLive: 'unknown',
      })
    ).toBe('connected');
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: ProxyErrorCode.SYSTEM_PROXY_FAILED,
        systemProxyLive: 'unknown',
      })
    ).toBe('proxy-degraded');
  });

  it('缺省（未接线的消费方不传该字段）等价于 unknown —— 接线前行为零回归', () => {
    expect(
      deriveTakeoverConnState({
        running: true,
        proxyModeType: 'systemProxy',
        errorCode: ProxyErrorCode.SYSTEM_PROXY_FAILED,
      })
    ).toBe('proxy-degraded');
    expect(
      deriveTakeoverConnState({ running: true, proxyModeType: 'systemProxy', errorCode: undefined })
    ).toBe('connected');
  });

  it('TUN/manual 不消费活态 —— 即便 not-effective 也算已连接', () => {
    // TUN 靠路由表夺流量、manual 只提供本地端口，系统代理指向谁与它们无关。
    // 若此处退化成「先看活态再看 mode」，TUN 用户会因为系统里另有第三方代理而被误报降级。
    for (const mode of ['tun', 'manual'] as const) {
      expect(
        deriveTakeoverConnState({
          running: true,
          proxyModeType: mode,
          errorCode: undefined,
          systemProxyLive: 'not-effective',
        })
      ).toBe('connected');
    }
  });

  it('核未运行时活态一律不参与 → disconnected（不得被 effective 拉成已连接）', () => {
    expect(
      deriveTakeoverConnState({
        running: false,
        proxyModeType: 'systemProxy',
        errorCode: undefined,
        systemProxyLive: 'effective',
      })
    ).toBe('disconnected');
  });

  it('isTrulyConnected：degraded 不算连上（防消费方 `!== disconnected` 写法复活误导）', () => {
    expect(isTrulyConnected('connected')).toBe(true);
    expect(isTrulyConnected('proxy-degraded')).toBe(false);
    expect(isTrulyConnected('disconnected')).toBe(false);
  });
});
