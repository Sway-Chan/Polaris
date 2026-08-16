/**
 * `domain/taildrop.ts` 的判据测试。
 *
 * 三条不变式各自都有过对应的真实缺陷形态，钉住它们不是补覆盖率：
 *  - 可用性塌成布尔 ⇒ 做出「灰着但不说为什么」的入口（本仓为 allowInternet / resolveByName 记过两次）；
 *  - 角标取 waiting 而不是 unread ⇒ 角标永远消不掉；
 *  - 进度除零 ⇒ `NaN%`，而 NaN 与 0 在界面上是两回事。
 */

import { describe, expect, it } from 'vitest';
import type { TailscaleStatusEvent } from '@/contracts/tailscale-status';
import {
  receivingPercent,
  taildropAvailability,
  taildropBadgeCount,
  taildropErrorKey,
  TAILDROP_ERROR_FALLBACK_KEY,
} from './taildrop';

function status(over: Partial<TailscaleStatusEvent> = {}): TailscaleStatusEvent {
  return {
    serverId: 'ts1',
    backendState: 'Running',
    loggedIn: true,
    tailscaleIPs: ['100.64.0.1'],
    expired: false,
    peers: [],
    canShareFiles: true,
    waitingFileCount: 0,
    receivingFileCount: 0,
    unreadFileCount: 0,
    ...over,
  };
}

describe('taildropAvailability：三态，不是布尔', () => {
  it('登录且 tailnet 授了 file-sharing → ready', () => {
    expect(taildropAvailability(status())).toBe('ready');
  });

  it('没有状态帧 / 未登录 → offline（该去连接）', () => {
    expect(taildropAvailability(undefined)).toBe('offline');
    expect(taildropAvailability(status({ loggedIn: false }))).toBe('offline');
  });

  it('登录了但 tailnet 没授权 → notGranted（在本应用里做什么都没用）', () => {
    // 这一格与 offline 塌在一起就会让用户以为「再连一次就好了」，而实际要去 admin console。
    expect(taildropAvailability(status({ canShareFiles: false }))).toBe('notGranted');
  });

  it('旧核（无该字段，解码缺省 false）落到 notGranted 而不是 ready', () => {
    // 降级方向必须是保守的：换了没有 Taildrop 的核，收发本来也不成立。
    expect(taildropAvailability(status({ canShareFiles: false, unreadFileCount: 3 }))).toBe(
      'notGranted'
    );
  });
});

describe('taildropBadgeCount', () => {
  it('取未读数，不取待处理数', () => {
    // 读过但没删的文件仍在 waiting 里 —— 拿 waiting 当角标，用户没有任何办法让它归零。
    expect(taildropBadgeCount(status({ unreadFileCount: 2, waitingFileCount: 7 }))).toBe(2);
    expect(taildropBadgeCount(status({ unreadFileCount: 0, waitingFileCount: 7 }))).toBe(0);
  });

  it('不可用时恒 0（连不上的收件箱不该在界面上顶着数字）', () => {
    expect(taildropBadgeCount(status({ loggedIn: false, unreadFileCount: 5 }))).toBe(0);
    expect(taildropBadgeCount(status({ canShareFiles: false, unreadFileCount: 5 }))).toBe(0);
    expect(taildropBadgeCount(undefined)).toBe(0);
  });

  it('负数被夹到 0（不信任下游给的计数）', () => {
    expect(taildropBadgeCount(status({ unreadFileCount: -1 }))).toBe(0);
  });
});

describe('taildropErrorKey：后端 code → i18n 键', () => {
  it('已登记的 code 各自换成自己的键', () => {
    expect(taildropErrorKey('TAILDROP_ENDPOINT_UNAVAILABLE')).toBe('taildrop.errUnavailable');
    expect(taildropErrorKey('TAILDROP_API_UNREACHABLE')).toBe('taildrop.errApi');
    expect(taildropErrorKey('TAILDROP_CALL_FAILED')).toBe('taildrop.errCall');
    expect(taildropErrorKey('TAILDROP_WRITE_FAILED')).toBe('taildrop.errWrite');
  });

  it('未知 code / 无 code → 兜底键，而不是把英文诊断显示给用户', () => {
    expect(taildropErrorKey(undefined)).toBe(TAILDROP_ERROR_FALLBACK_KEY);
    expect(taildropErrorKey('SOMETHING_NEW')).toBe(TAILDROP_ERROR_FALLBACK_KEY);
    expect(taildropErrorKey('')).toBe(TAILDROP_ERROR_FALLBACK_KEY);
  });
});

describe('receivingPercent', () => {
  it('常规比例四舍五入到整数', () => {
    expect(receivingPercent(250, 1000)).toBe(25);
    expect(receivingPercent(1, 3)).toBe(33);
  });

  it('size 未知（核先报 0）→ 0 而不是 NaN', () => {
    // NaN 会把进度条渲染成空白，而空白与「0%」在界面上是两回事。
    expect(receivingPercent(0, 0)).toBe(0);
    expect(receivingPercent(100, 0)).toBe(0);
    expect(receivingPercent(100, Number.NaN)).toBe(0);
  });

  it('越界值夹到 0..100', () => {
    expect(receivingPercent(2000, 1000)).toBe(100);
    expect(receivingPercent(-5, 1000)).toBe(0);
  });
});
