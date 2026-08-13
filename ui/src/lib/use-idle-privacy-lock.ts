/**
 * 自动隐私锁闲置计时（原型 Settings「自动隐私锁 · 闲置 10 分钟」）。
 *
 * 仅当 `autoPrivacyMode` 开 且 当前未锁 时武装：N 分钟无用户操作 → `config_set_privacy_mode(true)`
 * → 后端 emit enterPrivacyMode → App 全局订阅收敛 store.privacyMode → LockOverlay 遮罩。
 * 锁定后 privacyMode 变 true → 本 effect 依赖变化重跑 → shouldArmIdleLock 返 false → 清计时（不重复触发）。
 * 解锁后 privacyMode 变 false → 重新武装。挂在主窗根（App）一处，覆盖全窗口活动。
 */

import { useEffect } from 'react';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { api } from '@/ipc';
import { IDLE_PRIVACY_LOCK_MS, shouldArmIdleLock } from '@/domain/privacy';

const ACTIVITY_EVENTS = [
  'mousemove',
  'mousedown',
  'keydown',
  'wheel',
  'touchstart',
  'scroll',
] as const;

export function useIdlePrivacyLock(): void {
  const autoPrivacyMode = useEffectiveConfig((c) => c?.autoPrivacyMode ?? false);
  const privacyMode = useAppStore((s) => s.privacyMode);

  useEffect(() => {
    if (!shouldArmIdleLock(autoPrivacyMode, privacyMode)) return;

    let timer: ReturnType<typeof setTimeout>;
    const arm = () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        // 失败静默：进隐私态失败不该冒泡成 UI 异常；下一次活动/计时重试。
        void api.config.setPrivacyMode(true).catch(() => {});
      }, IDLE_PRIVACY_LOCK_MS);
    };

    ACTIVITY_EVENTS.forEach((e) =>
      window.addEventListener(e, arm, { passive: true }),
    );
    arm(); // 挂载即起计时（对齐「进入页面即开始计闲置」）

    return () => {
      clearTimeout(timer);
      ACTIVITY_EVENTS.forEach((e) => window.removeEventListener(e, arm));
    };
  }, [autoPrivacyMode, privacyMode]);
}
