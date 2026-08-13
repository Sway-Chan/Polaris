/**
 * 浮层状态四态折算（A2 的浮层半）+ 与**原生托盘图标**同一条优先级的一致性锁。
 *
 * 被守的缺陷不是"算错了"，而是"只有两态"：修复前浮层写死 `running ? 'ok' : 'idle'`，
 * 与 Rust 侧同样只有 connected 二态的图标一起，把「起核中」和「核崩溃」都呈现成"未连接"。
 * 故这里除了逐分支断言，还钉两件更容易在后续改动中悄悄退化的事：
 *  1. 四态必须映射到**四个互不相同**的 `.dot` 修饰类（有人图省事把 connecting 复用成 idle 就必须转红）；
 *  2. 优先级必须与 `src-tauri/src/tray.rs::resolve_tray_state` 逐分支同构（两面各说各话是最难查的 bug）。
 */
import { describe, it, expect } from 'vitest';
import { trayStatusTone, TRAY_TONE_DOT_CLASS } from './tray-status-tone';

describe('trayStatusTone —— 四态优先级（与 Rust resolve_tray_state 同构）', () => {
  it('running 压过一切：活核上的非致命错误不得把状态显示成异常', () => {
    // `set_nonfatal_error`（如 A1 的 SYSTEM_PROXY_FAILED）会在**活核**上留 errorCode。
    expect(trayStatusTone({ running: true, starting: false, errored: true })).toBe('connected');
    expect(trayStatusTone({ running: true, starting: true, errored: true })).toBe('connected');
    expect(trayStatusTone({ running: true, starting: false, errored: false })).toBe('connected');
  });

  it('starting 压过 errored：新一轮起核在飞时不该被上一轮的失败盖住', () => {
    expect(trayStatusTone({ running: false, starting: true, errored: true })).toBe('connecting');
    expect(trayStatusTone({ running: false, starting: true, errored: false })).toBe('connecting');
  });

  it('errored 压过 idle —— 这正是被修的缺口（崩溃此前与主动断开完全同形）', () => {
    expect(trayStatusTone({ running: false, starting: false, errored: true })).toBe('error');
  });

  it('三位皆假 = 未连接', () => {
    expect(trayStatusTone({ running: false, starting: false, errored: false })).toBe('idle');
  });
});

describe('degraded —— running 分支内部的再分叉（2026-07-28 复审补）', () => {
  // 被守的缺陷：浮层只有 `running → connected`，而主窗对同一状态展示「系统代理未生效」（琥珀）。
  // OS 代理被用户手改时，两个窗在同一时刻说相反的话。
  it('running + degraded → degraded（不得再报「已连接」）', () => {
    expect(trayStatusTone({ running: true, starting: false, errored: false, degraded: true })).toBe(
      'degraded'
    );
  });

  it('degraded 仍在 running 分支内：压过 starting / errored', () => {
    expect(trayStatusTone({ running: true, starting: true, errored: true, degraded: true })).toBe(
      'degraded'
    );
  });

  it('核没跑时 degraded 无意义 —— 不得把 disconnected/连接中说成降级', () => {
    expect(trayStatusTone({ running: false, starting: false, errored: false, degraded: true })).toBe(
      'idle'
    );
    expect(trayStatusTone({ running: false, starting: true, errored: false, degraded: true })).toBe(
      'connecting'
    );
  });

  it('不传 degraded → 与本改动前逐字节相同（缺省 false）', () => {
    expect(trayStatusTone({ running: true, starting: false, errored: false })).toBe('connected');
  });
});

describe('状态点类映射 —— 每一态必须视觉可辨', () => {
  const TONES = ['connected', 'degraded', 'connecting', 'error', 'idle'] as const;

  it('每一态都有类，且都用既有词汇表（不新造 CSS）', () => {
    // 这四个修饰类在 ui/src/styles/components.css:70-75 已存在；托盘另造一套会让同一语义在不同屏上长得不一样。
    const VOCAB = new Set(['ok', 'warn', 'err', 'idle']);
    expect(Object.keys(TRAY_TONE_DOT_CLASS).sort()).toEqual([...TONES].sort());
    for (const tone of TONES) {
      const cls = TRAY_TONE_DOT_CLASS[tone];
      expect(cls, `${tone} 缺状态点类`).toBeTruthy();
      expect(VOCAB.has(cls), `${tone} → .dot.${cls} 不在既有词汇表内`).toBe(true);
    }
  });

  it('三条「注意/正常/错误」色阶互不相同（把 connecting/error 折回 idle 必须转红）', () => {
    // 变异锁：A2 的「起核中有反馈、错误态可辨」被偷偷折回 idle 就在这里说话。
    const distinct = new Set([
      TRAY_TONE_DOT_CLASS.connected,
      TRAY_TONE_DOT_CLASS.connecting,
      TRAY_TONE_DOT_CLASS.error,
      TRAY_TONE_DOT_CLASS.idle,
    ]);
    expect(distinct.size).toBe(4);
  });

  it('degraded 与主窗 StatusBar 的 proxy-degraded 同色阶（warn），靠文案而非色阶区分于 connecting', () => {
    // 跨窗同语义必须同色阶：主窗 StatusBar.tsx 对 proxy-degraded 用的就是 warn。
    // 这条**故意**允许 degraded/connecting 共用 warn —— 上一条的「四色互不相同」不含 degraded。
    expect(TRAY_TONE_DOT_CLASS.degraded).toBe('warn');
  });
});
