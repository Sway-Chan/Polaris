/**
 * `relative-time` 的档位边界 —— 这四档是两个屏共用的契约，改阈值必须先来这里改。
 *
 * 断言打在**键**上而不是译文上：这里验的是「哪个区间落到哪一档」，译文对不对由
 * `locale-parity` / `i18n-coverage` 管。`t` 用一个把 key 与插值原样回吐的桩，
 * 于是插值参数（`count`）也一起被钉住 —— 少传一个 count 就会显示成 `{{count}} 小时前`。
 */
import { describe, it, expect } from 'vitest';
import { relativeTimeText, relativeTimeTextIso } from './relative-time';

const NOW = Date.UTC(2026, 7, 7, 12, 0, 0);
const H = 3_600_000;
/** 桩：回吐 `key` 与插值，形如 `common.relHoursAgo{count:3}`。 */
const t = (key: string, opts?: Record<string, unknown>) =>
  opts ? `${key}{${Object.entries(opts).map(([k, v]) => `${k}:${String(v)}`).join(',')}}` : key;

describe('relativeTimeText 四档边界', () => {
  it.each([
    ['刚过去', 0, 'common.relJustNow'],
    ['59 分钟', 59 * 60 * 1000, 'common.relJustNow'],
    ['整 1 小时 = 进入小时档', H, 'common.relHoursAgo{count:1}'],
    ['23 小时', 23 * H, 'common.relHoursAgo{count:23}'],
    ['整 24 小时 = 进入昨天档', 24 * H, 'common.relYesterday'],
    ['47 小时', 47 * H, 'common.relYesterday'],
    ['整 48 小时 = 进入天档', 48 * H, 'common.relDaysAgo{count:2}'],
    ['9 天', 9 * 24 * H, 'common.relDaysAgo{count:9}'],
  ])('%s', (_name, ago, want) => {
    expect(relativeTimeText(NOW - ago, t, NOW)).toBe(want);
  });

  it('未来时间戳（时钟回拨/服务端时间超前）落「刚刚」而不是负数小时', () => {
    expect(relativeTimeText(NOW + 5 * H, t, NOW)).toBe('common.relJustNow');
  });
});

describe('relativeTimeTextIso', () => {
  it('ISO 串按同一档位换算', () => {
    expect(relativeTimeTextIso(new Date(NOW - 3 * H).toISOString(), t, NOW)).toBe(
      'common.relHoursAgo{count:3}',
    );
  });

  it('缺省 / 不可解析 → 空串（调用点按「没有这个时间」渲染，不该显示 NaN）', () => {
    expect(relativeTimeTextIso(undefined, t, NOW)).toBe('');
    expect(relativeTimeTextIso('not-a-date', t, NOW)).toBe('');
  });
});
