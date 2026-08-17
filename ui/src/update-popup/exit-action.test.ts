/**
 * 退出动作 ↔ 后端 phase 白名单的对拍门。
 *
 * 这一层挡的是「渲染端发了一个后端会静默拒收的动作」= 死键。它不会让任何别的测试转红，
 * 只有用户在下载中按 Esc、发现什么都没发生时才暴露。
 */
import { describe, it, expect } from 'vitest';
import { exitActionFor } from './exit-action';
import type { PopupPhase } from '@/contracts/types/update';

/** 后端白名单快照（`crates/updater/src/popup.rs::is_valid_for`）。改了那边，本表必须同步。 */
const BACKEND_WHITELIST: Record<PopupPhase, readonly string[]> = {
  remind: ['update', 'later', 'skip', 'viewLog'],
  progress: ['cancel'],
  error: ['retry', 'manualDownload', 'close'],
  done: ['close'],
  noupdate: ['close'],
};

describe('exitActionFor：退出动作必须在后端该 phase 的白名单里', () => {
  for (const phase of Object.keys(BACKEND_WHITELIST) as PopupPhase[]) {
    it(`${phase} 态的退出动作被后端接受`, () => {
      // 变异对照：把实现改回「恒返回 'close'」→ remind / progress 两条转红（正是修复前的形态）。
      expect(BACKEND_WHITELIST[phase]).toContain(exitActionFor(phase));
    });
  }

  it('progress 态发 cancel 而不是 close（close 会被静默忽略 = 死键）', () => {
    expect(exitActionFor('progress')).toBe('cancel');
  });

  it('phase 未知时仍给出一个动作（逃生优先，绝不返回空）', () => {
    expect(exitActionFor(undefined)).toBe('close');
  });
});
