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
import { i18nReady } from './src/i18n';
import './src/styles/index.css';
import App from './src/App';
import { DEMO_CONFIG, DEMO_SERVERS } from './harness-fixture';
import { TOPOLOGY_OTHERS_KEY, type ConnectionsAggregate } from './src/contracts/types';

/**
 * 首页连接流向的保真数据源：64 个目标足以覆盖默认 16 槽与最大化运行态 40 槽。
 * 先在完整集合过滤、再按调用方 slots 投影；harness 也不能把绘制预算误装成检索真值。
 */
const DEMO_TOPOLOGY_TARGETS = Array.from({ length: 64 }, (_, index) => ({
  name: `target-${String(index + 1).padStart(2, '0')}.example`,
  count: 64 - index,
}));

function demoTopology(payload: unknown): ConnectionsAggregate {
  const args = payload as { query?: string; slots?: number } | null;
  const query = args?.query?.trim().toLowerCase() ?? '';
  const slots = Math.max(4, Math.min(128, Math.floor(args?.slots ?? 16)));
  const matching = DEMO_TOPOLOGY_TARGETS.filter((target) => target.name.toLowerCase().includes(query));
  const total = matching.reduce((sum, target) => sum + target.count, 0);
  const overflow = matching.length > slots;
  const visibleCount = overflow ? slots - 1 : matching.length;
  const recentCount = overflow ? Math.ceil(visibleCount / 3) : 0;
  const mainCount = visibleCount - recentCount;
  const main = matching.slice(0, mainCount);
  const recent = matching.slice(mainCount, visibleCount);
  const hidden = matching.slice(visibleCount);
  const hosts: ConnectionsAggregate['hosts'] = [
    ...main.map((target) => ({
      ...target,
      flows: [{ outbound: 'HK', count: target.count }],
      recent: false,
    })),
    ...recent.map((target) => ({
      ...target,
      flows: [{ outbound: 'HK', count: target.count }],
      recent: true,
    })),
  ];
  if (hidden.length > 0) {
    const hiddenCount = hidden.reduce((sum, target) => sum + target.count, 0);
    hosts.push({
      name: TOPOLOGY_OTHERS_KEY,
      count: hiddenCount,
      flows: [{ outbound: 'HK', count: hiddenCount }],
      recent: false,
    });
  }
  return {
    total,
    hosts,
    outbounds: total > 0 ? [{ name: 'HK', count: total }] : [],
    at: Date.now(),
  };
}

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
    case 'proxy_get_status': return Promise.resolve({ running: true, startTime: Date.now() - 300_000 });
    case 'stats_subscribe': return Promise.resolve(null);
    case 'stats_unsubscribe': return Promise.resolve(null);
    case 'stats_project_topology': return Promise.resolve(demoTopology(payload));
    case 'renderer_log': return Promise.resolve(null);
    case 'plugin:os|platform': return Promise.resolve('macos');
    default: return Promise.resolve(null);
  }
}, { shouldMockEvents: true }); // 开事件模拟：verify 脚本用 emit() 喂 EVENT_CONNECTIONS_AGGREGATE 等推送事件

void i18nReady.then(() => {
  ReactDOM.createRoot(document.getElementById('root')!).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>,
  );
});
