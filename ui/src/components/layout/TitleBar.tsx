/**
 * 窗口控制（titlebar 溶进 sidebar/main，原型 L104-126）。
 *
 * 原型把窗口控制分两套，按 [data-os] 显隐：
 *  - macOS：原生交通灯（decorations:true，OS 绘制），叠在 sidebar 顶部（Overlay 标题栏）。
 *           前端不自绘，仅 Sidebar 的 .side-chrome 留 36px 净空 + 拖动区。
 *  - Win/Linux：min/max/close 浮在 main 右上角（.winctl，绝对定位，decorations:false 下唯一关闭入口）。
 *
 * 故本组件仅导出 WinCtl（Win/Linux）。视觉全走 components.css 的 .winctl 语义类（含 :root[data-os]
 * 显隐 —— mac 下 display:none）。
 *
 * WinCtl 按钮接 window_minimize / window_maximize_toggle / window_close。最大化态经
 * window_is_maximized 水合 + event:windowMaximizeChanged 订阅跟随（后端按钮 / WM 双击标题栏 / 拖顶
 * 均走同一广播，故不在点击处本地乐观更新，保单一真值源）。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { WinMinIcon, WinMaxIcon, WinRestoreIcon, WinCloseIcon } from '@/components/icons';
import { api } from '@/ipc';

/** Win/Linux 浮动窗口控制（min/max/close）。原型 .winctl L118-126。 */
export function WinCtl({ className }: { className?: string }) {
  const { t } = useTranslation();
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    api.window
      .isMaximized()
      .then(setMaximized)
      .catch((err) => console.error('[WinCtl] isMaximized failed:', err));
    return api.window.onMaximizeChanged((data) => setMaximized(data.maximized));
  }, []);

  return (
    <span className={cn('winctl', className)}>
      <button
        type="button"
        onClick={() => void api.window.minimize().catch((err) => console.error('[WinCtl] minimize failed:', err))}
        aria-label={t('common.windowMinimize')}
      >
        <WinMinIcon />
      </button>
      <button
        type="button"
        onClick={() => void api.window.maximizeToggle().catch((err) => console.error('[WinCtl] maximizeToggle failed:', err))}
        aria-label={maximized ? t('common.windowRestore') : t('common.windowMaximize')}
      >
        {maximized ? <WinRestoreIcon /> : <WinMaxIcon />}
      </button>
      <button
        type="button"
        className="cls"
        onClick={() => void api.window.close().catch((err) => console.error('[WinCtl] close failed:', err))}
        aria-label={t('common.close')}
      >
        <WinCloseIcon />
      </button>
    </span>
  );
}
