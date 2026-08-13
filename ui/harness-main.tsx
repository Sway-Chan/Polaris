/**
 * 保真验证 harness（仅本机验证用，不进产物）：mockIPC 喂 demo 数据 → 渲染真实 <App>。
 * headless Blink 下 nav 可点、下拉/弹窗可开，用于逐屏对拍原型 + 验证修复。
 * 命令字符串取自 domain/ipc-channels.ts。
 *
 * demo 数据本体在 `./harness-fixture`——拆出去的理由（以及它为什么必须带类型标注）见那个文件的头注：
 * 数据留在本文件里 = 任何自动化都碰不到它（本文件 import 即执行 createRoot），契约漂移只能等真机白屏。
 */
import React from 'react';
import ReactDOM from 'react-dom/client';
import { mockIPC } from '@tauri-apps/api/mocks';
import App from './src/App';
import { DEMO_CONFIG, DEMO_SERVERS } from './harness-fixture';
import './src/styles/index.css';
import './src/i18n';

mockIPC((cmd, payload) => {
  switch (cmd) {
    case 'config_get': return Promise.resolve(DEMO_CONFIG);
    case 'config_get_privacy_mode': return Promise.resolve(false);
    case 'config_get_value': return Promise.resolve((DEMO_CONFIG as unknown as Record<string, unknown>)[(payload as { key: string })?.key] ?? null);
    case 'server_get_all': return Promise.resolve(DEMO_SERVERS);
    case 'app_presets_list': return Promise.resolve([]);
    case 'rule_resources_list': return Promise.resolve([]);
    // 契约是 RuleResourceCatalogResult（{items, fetchedAt, source}），不是裸数组——回 [] 会让
    // 资源库/添加应用两个弹窗读 `catalog.items.filter` 时炸在 undefined 上。
    case 'rule_resources_get_catalog': return Promise.resolve({ items: [], fetchedAt: null, source: 'builtin' });
    case 'stats_subscribe': return Promise.resolve(null);
    case 'renderer_log': return Promise.resolve(null);
    case 'plugin:os|platform': return Promise.resolve('macos');
    default: return Promise.resolve(null);
  }
}, { shouldMockEvents: true }); // 开事件模拟：verify 脚本用 emit() 喂 EVENT_CONNECTIONS_AGGREGATE 等推送事件

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
