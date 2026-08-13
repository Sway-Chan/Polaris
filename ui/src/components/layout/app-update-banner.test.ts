/**
 * AppUpdateBanner 纯逻辑单测 —— 锁死「跳过的版本不再提示」与「查失败不当有更新」两条。
 *
 * 这两条各自对应一个真实故障形态：
 *  · 跳过后仍提示 ⇒ 「跳过此版本」按钮形同虚设（用户点了没反应，本会话反复被打扰）；
 *  · 检查失败/无版本号却渲染 ⇒ 横幅在没有更新时挂着，属 B5 反伪造直接命中的一类假 UI。
 */
import { describe, it, expect, beforeEach } from 'vitest';
import {
  appUpdateBannerState,
  markAppVersionSkipped,
  shouldBannerCheckUpdate,
  skippedAppVersions,
  subscribeAppVersionSkipped,
  __resetSkippedForTest,
} from './app-update-banner';

const EMPTY: ReadonlySet<string> = new Set<string>();

describe('appUpdateBannerState', () => {
  it('有更新且未跳过未关闭 → 渲染并带版本号', () => {
    expect(
      appUpdateBannerState({
        snapshot: { hasUpdate: true, version: '0.2.0' },
        skipped: EMPTY,
        dismissed: EMPTY,
      })
    ).toEqual({ visible: true, version: '0.2.0' });
  });

  it('无快照（尚未查 / 查失败）→ 不渲染', () => {
    expect(
      appUpdateBannerState({ snapshot: null, skipped: EMPTY, dismissed: EMPTY })
    ).toEqual({ visible: false, version: null });
  });

  it('hasUpdate 为 false → 不渲染', () => {
    expect(
      appUpdateBannerState({
        snapshot: { hasUpdate: false, version: '0.2.0' },
        skipped: EMPTY,
        dismissed: EMPTY,
      }).visible
    ).toBe(false);
  });

  it('hasUpdate 为真但缺版本号（后端契约破损）→ 不渲染空版本横幅', () => {
    expect(
      appUpdateBannerState({
        snapshot: { hasUpdate: true, version: null },
        skipped: EMPTY,
        dismissed: EMPTY,
      }).visible
    ).toBe(false);
  });

  it('该版本已被跳过 → 不渲染（本会话不得再提示）', () => {
    expect(
      appUpdateBannerState({
        snapshot: { hasUpdate: true, version: '0.2.0' },
        skipped: new Set(['0.2.0']),
        dismissed: EMPTY,
      }).visible
    ).toBe(false);
  });

  it('跳过的是别的版本 → 照常渲染（跳过是按版本粒度，不是总开关）', () => {
    expect(
      appUpdateBannerState({
        snapshot: { hasUpdate: true, version: '0.3.0' },
        skipped: new Set(['0.2.0']),
        dismissed: EMPTY,
      }).visible
    ).toBe(true);
  });

  it('用户关闭横幅 → 不渲染', () => {
    expect(
      appUpdateBannerState({
        snapshot: { hasUpdate: true, version: '0.2.0' },
        skipped: EMPTY,
        dismissed: new Set(['0.2.0']),
      }).visible
    ).toBe(false);
  });
});

describe('会话级 skipped 集合', () => {
  beforeEach(() => __resetSkippedForTest());

  it('记入后可被读到，并通知订阅者', () => {
    let hits = 0;
    const off = subscribeAppVersionSkipped(() => {
      hits += 1;
    });
    markAppVersionSkipped('0.2.0');
    expect(skippedAppVersions().has('0.2.0')).toBe(true);
    expect(hits).toBe(1);
    off();
  });

  it('重复记入不再通知（防同一版本反复触发重渲）', () => {
    let hits = 0;
    const off = subscribeAppVersionSkipped(() => {
      hits += 1;
    });
    markAppVersionSkipped('0.2.0');
    markAppVersionSkipped('0.2.0');
    expect(hits).toBe(1);
    off();
  });

  it('空版本号忽略（后端 update_skip 对空 version 直接报错）', () => {
    markAppVersionSkipped('');
    expect(skippedAppVersions().size).toBe(0);
  });

  it('退订后不再收到通知', () => {
    let hits = 0;
    const off = subscribeAppVersionSkipped(() => {
      hits += 1;
    });
    off();
    markAppVersionSkipped('0.2.0');
    expect(hits).toBe(0);
  });
});

describe('shouldBannerCheckUpdate（与后端 should_auto_check_update 同口径）', () => {
  it('config 未载入 → 不查（避免「已关掉却仍发一次请求」）', () => {
    expect(shouldBannerCheckUpdate(null)).toBe(false);
  });

  it('缺省（未设该键）→ 查（缺省为开，对齐后端 != Some(false)）', () => {
    expect(shouldBannerCheckUpdate({})).toBe(true);
  });

  it('显式 true → 查', () => {
    expect(shouldBannerCheckUpdate({ autoCheckUpdate: true })).toBe(true);
  });

  it('显式 false → 不查', () => {
    expect(shouldBannerCheckUpdate({ autoCheckUpdate: false })).toBe(false);
  });
});
