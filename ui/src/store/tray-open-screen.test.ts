/**
 * A1：托盘「打开设置」的主窗侧消费点（窄跨窗导航通道）。
 *
 * 通道的值域由 Rust 白名单 `tray::normalize_tray_screen` 钉死，本侧再守一道：**未登记值一律不导航**。
 * 两道白名单不是重复——Rust 那道保证「发出去的只能是登记值」，这道保证「就算通道被别的东西污染
 * （或将来有人在前端另发一次），也不会跳到没预期的屏」。宁可不跳，也不跳错。
 */
import { describe, it, expect, beforeEach } from 'vitest';
import { useNavStore, applyTrayScreenIntent } from './nav-store';

beforeEach(() => {
  useNavStore.setState({ scope: 'main', mainScreen: 'home', settingsScreen: 'general' });
});

describe('applyTrayScreenIntent', () => {
  it("'settings' → 进设置 scope", () => {
    expect(applyTrayScreenIntent('settings')).toBe(true);
    expect(useNavStore.getState().scope).toBe('settings');
  });

  it('未登记值一律忽略，且不动导航态', () => {
    for (const evil of ['home', 'nodes', '/settings', 'Settings', '', null, undefined, 42, {}]) {
      expect(applyTrayScreenIntent(evil), `${String(evil)} 不该被放行`).toBe(false);
    }
    const s = useNavStore.getState();
    expect(s.scope).toBe('main');
    expect(s.mainScreen).toBe('home');
  });

  it('复用既有 enterSettings 语义（默认落 general 子页，不另造一套导航写法）', () => {
    useNavStore.setState({ settingsScreen: 'about' });
    applyTrayScreenIntent('settings');
    expect(useNavStore.getState().settingsScreen).toBe('general');
  });
});
