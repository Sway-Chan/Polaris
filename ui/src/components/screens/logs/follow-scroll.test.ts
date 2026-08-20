import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import {
  isLogViewAtBottom,
  isUpwardLogScrollKey,
  shouldPauseLogFollow,
  USER_SCROLL_INTENT_WINDOW_MS,
} from './follow-scroll';

const metrics = (distanceFromBottom: number) => ({
  scrollHeight: 1000,
  clientHeight: 400,
  scrollTop: 600 - distanceFromBottom,
});

describe('日志 follow 的滚动来源判据', () => {
  it('首次水合/程序化滚动没有用户意图，即使事件先看到离底也不暂停', () => {
    expect(
      shouldPauseLogFollow({
        follow: true,
        metrics: metrics(500),
        lastUserIntentAt: null,
        now: 1000,
      })
    ).toBe(false);
  });

  it('用户刚滚动且已离底才暂停；贴底、已暂停与过期意图均不暂停', () => {
    expect(
      shouldPauseLogFollow({
        follow: true,
        metrics: metrics(31),
        lastUserIntentAt: 1000,
        now: 1001,
      })
    ).toBe(true);
    expect(
      shouldPauseLogFollow({
        follow: true,
        metrics: metrics(30),
        lastUserIntentAt: 1000,
        now: 1001,
      })
    ).toBe(false);
    expect(
      shouldPauseLogFollow({
        follow: false,
        metrics: metrics(31),
        lastUserIntentAt: 1000,
        now: 1001,
      })
    ).toBe(false);
    expect(
      shouldPauseLogFollow({
        follow: true,
        metrics: metrics(31),
        lastUserIntentAt: 1000,
        now: 1000 + USER_SCROLL_INTENT_WINDOW_MS + 1,
      })
    ).toBe(false);
  });

  it('键盘只把向历史方向的动作认作暂停意图', () => {
    for (const key of ['ArrowUp', 'PageUp', 'Home']) {
      expect(isUpwardLogScrollKey(key)).toBe(true);
    }
    expect(isUpwardLogScrollKey(' ', true)).toBe(true);
    for (const key of ['ArrowDown', 'PageDown', 'End', ' ']) {
      expect(isUpwardLogScrollKey(key)).toBe(false);
    }
    expect(isLogViewAtBottom(metrics(30))).toBe(true);
    expect(isLogViewAtBottom(metrics(31))).toBe(false);
  });

  it('生产接线保留 150ms 合批与 500 行预算，并覆盖四类用户滚动输入', () => {
    const here = dirname(fileURLToPath(import.meta.url));
    const screen = readFileSync(resolve(here, 'LogsScreen.tsx'), 'utf8');
    const backend = readFileSync(resolve(here, '../../../../../src-tauri/src/commands/misc.rs'), 'utf8');
    expect(backend).toContain('const LOG_BATCH_INTERVAL_MS: u64 = 150;');
    expect(screen).toContain('const MAX_RENDERED_ROWS = 500;');
    expect(screen).toContain('shouldPauseLogFollow({');
    for (const event of ['wheel', 'touchstart', 'pointerdown', 'pointermove', 'keydown']) {
      expect(screen).toContain(`addEventListener('${event}'`);
    }
  });
});
