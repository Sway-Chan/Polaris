/**
 * 主窗 renderer-ready 的文档级单一出口。
 *
 * 同一 WebView 文档只需向 mount 健康门证明一次“真实页面已提交”。模块状态会随轻量态 WebView 销毁而
 * 自然复位；失败后解除去重，ErrorBoundary 或后续路由提交仍有机会补发。此信号不携用户文案。
 */

import { invoke } from '@tauri-apps/api/core';
import { IPC_CHANNELS } from '@/domain/ipc-channels';

// 不复用 settings-logic 的 `createOnceGate`：它是不可逆的一次性闸，而这里 IPC reject 后必须允许补发；
// 同时 ready 属于最小启动链，不应为一个布尔把设置域逻辑拖进主 bundle。
let reported = false;

export function reportRendererReady(): void {
  if (reported) return;
  reported = true;
  try {
    void invoke(IPC_CHANNELS.RENDERER_READY).catch(() => {
      reported = false;
    });
  } catch {
    reported = false;
  }
}
