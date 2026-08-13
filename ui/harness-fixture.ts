/**
 * 保真验证 harness 的 demo 数据（仅本机验证用，不进产物）。
 *
 * # 为什么从 harness-main.tsx 里拆出来
 *
 * 原先它和 `mockIPC` / `createRoot` 挤在同一个文件里，而那个文件在 node 下 import 即炸
 * （`document.getElementById` / `mockIPC` 都要 window）。于是**没有任何自动化能碰到这份数据** ——
 * 2026-07-30 发现 `customRules` 停在早已作废的旧 Rule schema（`conditions[].value` / `logic` /
 * `target`），`RuleItem` 读 `c.values.join()` 当场 TypeError、整棵 React 树卸载，「规则」屏及其之后
 * 所有屏在 harness 里静默不可达。数据与引导拆开后，`src/harness-screens.test.tsx` 才可能逐屏渲染它。
 *
 * # 两条硬约束（少一条这份 fixture 就会再次静默漂移）
 *
 *  1. **必须带类型标注**（`UserConfig` / `ServerConfig[]`）。只把文件纳入 tsconfig 是不够的 ——
 *     裸对象字面量没有上下文类型，契约改了它一个字都不会红。类型标注才是让 `tsc` 咬住它的那颗牙。
 *  2. **契约漂移之外的「类型对但渲染炸」由渲染冒烟门兜**（见 `src/harness-screens.test.tsx`）。
 */

import type { ServerConfig, UserConfig } from '@/contracts/types';

/** 订阅拉取时间：沿用旧 fixture 的「刚更新」语义（原为 `Date.now()`），使订阅条显示相对新鲜。 */
const NOW_ISO = new Date().toISOString();

export const DEMO_SERVERS: ServerConfig[] = [
  { id: 's1', name: '香港 IEPL · 01', protocol: 'vless', address: 'hk01.iepl.example.net', port: 443, uuid: 'demo-uuid-1', flow: 'xtls-rprx-vision', encryption: 'none', subscriptionId: 'sub1' },
  { id: 's2', name: '日本 · 东京 02', protocol: 'trojan', address: 'jp02.example.net', port: 443, password: 'demo', subscriptionId: 'sub1' },
  { id: 's3', name: '自建 · 新加坡', protocol: 'vless', address: 'sg.example.net', port: 8443, uuid: 'demo-uuid-3', encryption: 'none' },
];

export const DEMO_CONFIG: UserConfig = {
  subscriptions: [{ id: 'sub1', name: 'IEPL 机场', url: 'https://sub.example.net/link', autoUpdate: true, createdAt: NOW_ISO, lastUpdated: NOW_ISO }],
  servers: DEMO_SERVERS,
  selectedServerId: 's1',
  proxyMode: 'smart',
  proxyModeType: 'tun',
  tunConfig: { mtu: 1350, stack: 'auto', autoRoute: true, strictRoute: true },
  tunStackMigrated: true,
  // 单条件规则一律不写 `conditions`（契约：≥2 条件才存在，首条件恒镜像到 type/values），
  // 与旧 fixture 的「一条 geosite + 两个值」逐项对应，不借修 schema 之名改变展示面。
  customRules: [
    { id: 'r1', remarks: '流媒体解锁', type: 'geosite', values: ['netflix', 'disney'], action: 'proxy', targetServerId: 's1', enabled: true },
    { id: 'r2', remarks: 'Adblock', type: 'ruleSet', values: ['category-ads-all'], action: 'block', enabled: true },
  ],
  autoStart: false, silentStart: false, autoConnect: false, minimizeToTray: true,
  autoCheckUpdate: true, autoLightweightMode: false, hardwareAcceleration: true, windowEffects: true,
  desktopNotifications: true, autoUpdateSubscriptionOnStart: true, subscriptionUpdateIntervalHours: 12,
  subscriptionProxyPolicy: 'follow', mainSessionViaProxy: true, rememberWindowSize: true,
  interruptConnectionsOnSwitch: true, enableIPv6: false, autoPrivacyMode: false, privacyPassword: '',
  dnsConfig: { domesticDns: 'https://doh.pub/dns-query', foreignDns: 'https://dns.google/dns-query', enableFakeIp: true, fakeIpToggleMigrated: true, fakeIpTunAutoEnable: false, takeoverSystemDns: true, nodeResolverPool: ['ali', 'dnspod'], nodeResolverSingle: 'ali', nodeResolverMigrated: true },
  customRuleSets: [], appRulesSeeded: true, appRoutingEnabled: true,
  ruleResourceAutoUpdate: true, ruleResourceUpdateIntervalHours: 12, fakeIpFilter: true, blockQuic: true,
  singboxDashboard: true, mixedPort: 7890, controlPort: 9090, logLevel: 'info', disableLogFile: false,
  uiTheme: 'system', language: 'auto',
  // 旧 fixture 在这里带着 id/name/category/matchType/matchValue/processNames —— 那是**预设表**的字段，
  // 早已归 Rust SoT（`app_presets_list`）。AppRule 现在只剩「哪个预设、走哪条腿」四个字段。
  appRules: [
    { appId: 'github', action: 'proxy', enabled: true },
    { appId: 'google', action: 'proxy', enabled: true },
    { appId: 'spotify', action: 'proxy', enabled: true },
    { appId: 'netflix', action: 'proxy', targetServerId: 's1', enabled: true },
    { appId: 'youtube', action: 'proxy', enabled: true },
    { appId: 'telegram', action: 'direct', enabled: true },
  ],
  customAppPresets: [],
};
