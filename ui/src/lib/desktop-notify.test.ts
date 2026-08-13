/**
 * C8 桌面通知出口单测：门控（desktopNotifications 开关）+ 权限解析 + notify invoke payload。
 * 无 DOM 依赖——mock 裸 `@tauri-apps/api/core` invoke，验决策与调用形。
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';

const invokeMock = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import {
  notifyDesktop,
  setDesktopNotificationsEnabled,
  isDesktopNotificationsEnabled,
  __resetDesktopNotifyPermissionCache,
} from './desktop-notify';

const NOTIFY = 'plugin:notification|notify';
const IS_GRANTED = 'plugin:notification|is_permission_granted';
const REQUEST = 'plugin:notification|request_permission';

describe('desktopNotify (C8)', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setDesktopNotificationsEnabled(true);
    __resetDesktopNotifyPermissionCache();
  });

  it('权限已授予 + 开关开 → 发 notify（invoke plugin:notification|notify，payload options:{title,body}）', async () => {
    invokeMock.mockImplementation((cmd: string) =>
      cmd === IS_GRANTED ? Promise.resolve(true) : Promise.resolve()
    );
    await notifyDesktop('代理出错', '已停止');
    expect(invokeMock).toHaveBeenCalledWith(NOTIFY, {
      options: { title: '代理出错', body: '已停止' },
    });
  });

  it('desktopNotifications 关 → 不发（连权限都不查）', async () => {
    setDesktopNotificationsEnabled(false);
    expect(isDesktopNotificationsEnabled()).toBe(false);
    await notifyDesktop('T', 'B');
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('setDesktopNotificationsEnabled(undefined) 视为开（缺省/旧配置默认开）', () => {
    setDesktopNotificationsEnabled(undefined);
    expect(isDesktopNotificationsEnabled()).toBe(true);
  });

  it('权限未决（null）→ 请求一次；granted 才发', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === IS_GRANTED) return Promise.resolve(null);
      if (cmd === REQUEST) return Promise.resolve('granted');
      return Promise.resolve();
    });
    await notifyDesktop('T', 'B');
    expect(invokeMock).toHaveBeenCalledWith(REQUEST);
    expect(invokeMock).toHaveBeenCalledWith(NOTIFY, { options: { title: 'T', body: 'B' } });
  });

  it('权限被拒 → 不 notify', async () => {
    invokeMock.mockImplementation((cmd: string) =>
      cmd === IS_GRANTED ? Promise.resolve(false) : Promise.resolve()
    );
    await notifyDesktop('T', 'B');
    expect(invokeMock).not.toHaveBeenCalledWith(NOTIFY, expect.anything());
  });

  it('权限查询抛异常（非 Tauri / 插件异常）→ 静默不发，不抛', async () => {
    invokeMock.mockImplementation((cmd: string) =>
      cmd === IS_GRANTED ? Promise.reject(new Error('no tauri')) : Promise.resolve()
    );
    await expect(notifyDesktop('T', 'B')).resolves.toBeUndefined();
    expect(invokeMock).not.toHaveBeenCalledWith(NOTIFY, expect.anything());
  });

  it('权限解析缓存：第二次发不重复查权限', async () => {
    invokeMock.mockImplementation((cmd: string) =>
      cmd === IS_GRANTED ? Promise.resolve(true) : Promise.resolve()
    );
    await notifyDesktop('a', 'b');
    await notifyDesktop('c', 'd');
    const permChecks = invokeMock.mock.calls.filter((c) => c[0] === IS_GRANTED).length;
    expect(permChecks).toBe(1);
    const notifies = invokeMock.mock.calls.filter((c) => c[0] === NOTIFY).length;
    expect(notifies).toBe(2);
  });
});
