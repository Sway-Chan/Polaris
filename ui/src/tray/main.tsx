/**
 * 托盘自绘浮层渲染端入口（独立窗口，label=tray）。
 *
 * 独立入口（不复用 index.html）：浮层是 268px 的托盘菜单，复用主入口会把整个 React 应用（i18n/路由/
 * 全部 provider）挂进小窗，且主窗白屏自愈门（window_health.rs 只认 label=="main"）会误判。见 vite.config.ts
 * 多入口注释 + runtime/update_popup.rs 同款隔离理由。
 *
 * 首帧不依赖 IPC：浮层空态即「未连接 + 无节点」，hydrate 异步补真值；窗口显隐/定位/尺寸全由 Rust 侧
 * tray 模块驱动，前端只在内容变化时回报高度（TrayMenu 内 ResizeObserver → tray_resize）。
 */

import React from 'react';
import ReactDOM from 'react-dom/client';
import { invoke } from '@tauri-apps/api/core';
import TrayMenu from './TrayMenu';
import { applyTrayTheme } from './tray-theme';
import { disableNativeContextMenu } from '../lib/native-context-menu';
// 复用主窗全部设计令牌 + `.tray-*` / `.dot` / 品牌星规则（只读引用，不改 ui/src/styles）。
import '../styles/index.css';
// 浮层窗差异覆盖（透明 body + 卡片静态填充）。
import './tray-overlay.css';

/** 设 <html data-os>（tokens 的 transparent 分区 / 圆角驱动）：先 UA 兜底，再由 tauri-plugin-os 校正。 */
function applyOs(): void {
  const ua = navigator.userAgent.toLowerCase();
  const fallback = ua.includes('mac') ? 'mac' : ua.includes('win') ? 'win' : 'lin';
  document.documentElement.setAttribute('data-os', fallback);
  invoke<string>('plugin:os|platform')
    .then((p) => {
      const os = p === 'macos' ? 'mac' : p === 'windows' ? 'win' : 'lin';
      document.documentElement.setAttribute('data-os', os);
    })
    .catch(() => {
      /* 非 Tauri（浏览器预览）：保留 UA 兜底 */
    });
}

applyOs();
// 浮层是独立 webview，同样会弹系统右键菜单 —— 单独关一次（主窗那次管不到这个窗）。
disableNativeContextMenu();
// 首帧主题：config 未知 → 按系统偏好兜底（applyTrayTheme(undefined) 即跟随 prefers-color-scheme，
// tokens 默认深色）。TrayMenu hydrate 后按 config.uiTheme 精确校正，并接管「系统主题变化 / 每次弹出」
// 的实时跟随（防切系统主题后点托盘闪旧主题，见 TrayMenu 两处 effect）。
applyTrayTheme(undefined);

const root = document.getElementById('tray-root');
if (root) {
  ReactDOM.createRoot(root).render(
    <React.StrictMode>
      <TrayMenu />
    </React.StrictMode>
  );
}
