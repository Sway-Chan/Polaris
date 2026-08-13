/**
 * 托盘节点选择的纯逻辑（供 TrayMenu.tsx 消费 + vitest 直测）。
 *
 * 抽成独立模块而非留在 TrayMenu.tsx 里：vitest 走 node 环境（vite.config.ts 未引 jsdom），
 * TrayMenu 组件本身用了 document/ResizeObserver/matchMedia 测不了；纯逻辑分离出来才能钉住
 * 回归（同 node-edit-routing.ts / speedtest-feedback.ts 先例）。
 */

import { isSentinelSelection } from '@/domain/direct-selection';
import {
  deriveConnectButtonState,
  deriveProxyPhase,
  type ConnectButtonState,
} from '@/components/screens/home/connect-button-state';

/**
 * 托盘节点行的协议短标签（菜单宽度受限：长协议名缩短，保留延迟徽标的横向空间）。
 *
 * 1:1 移植自 上游 `TrayManager.ts` `PROTOCOL_SHORT`（wireguard→WG / tailscale→TS / shadowsocks→SS
 * / hysteria2→Hy2；其余协议已短，回退大写）。刻意**不**复用 NodeCard 的 `protocolLabel`（那是全名
 * VLESS/WireGuard/Tailscale，主窗节点卡够宽；托盘行窄，须用短写与 上游 托盘一致）。
 */
const PROTOCOL_SHORT: Record<string, string> = {
  wireguard: 'WG',
  tailscale: 'TS',
  shadowsocks: 'SS',
  hysteria2: 'Hy2',
};

/** 协议短标签：命中短写表取短写，否则大写（对齐 上游 `PROTOCOL_SHORT[...] ?? toUpperCase()`）。 */
export function protoShort(protocol: string | null | undefined): string {
  const p = (protocol ?? '').toLowerCase();
  return PROTOCOL_SHORT[p] ?? p.toUpperCase();
}

/**
 * 托盘「连接」按钮可用性判据里的「已配置」——修复此前 `canConnect` 只看 `servers.length>0`、
 * 与 Home 的 `serverConfigured = directSelected || !!currentServer?.id`（HomeScreen.tsx:459）
 * 口径分裂的问题：直连哨兵选中时不需要任何真实节点也算「已配置」，否则 direct-only 配置
 * 无法从托盘启动（TrayMenu.tsx 原 canConnect 只判 servers.length>0）。
 */
export function isTrayServerConfigured(
  selectedServerId: string | null | undefined,
  serverCount: number
): boolean {
  // 阻断哨兵同直连：不需要真实节点也算「已配置」（config-engine 已豁免其存在性校验，零节点可起核）。
  return isSentinelSelection(selectedServerId) || serverCount > 0;
}

/**
 * 测速超时后端记 -1（src-tauri/src/commands/speedtest.rs `measure_via_local_proxy`：
 * "超时/传输错/非2xx→None，上层记-1，绝不伪造数值"），与 `sortServersByLatency` 的
 * 「无结果」口径对齐（该函数把 `<0` 与 null/undefined 一视同仁沉底）。
 *
 * 直接把原始值喂给 `shared/format` 的 `latLevel`/`latText` 会把 -1 落进 `<80` 分支误判成
 * "fast"（绿色徽标）——这比不显示徽标更误导，故托盘侧先归一成 null（=「无有效结果」），
 * 复用该文件已建立的语义而非发明新的超时表示。
 */
export function normalizeLatency(
  v: number | null | undefined
): number | null | undefined {
  return v !== undefined && v !== null && v < 0 ? null : v;
}

/**
 * 托盘连接钮派生（**唯一决策点**，TrayMenu.tsx 只负责渲染 + 按 `action` 分发）。
 *
 * 收口到与主窗同一个 `deriveConnectButtonState`：托盘是独立窗口、不共享 Zustand store，两边各写一套
 * 判定必然分叉 —— 原来的 `running ? stop : start` 就是分叉出来的缺陷（TrayMenu.tsx 原 :219-236）。
 *
 * **起核在飞有两个来源，缺一不可**：
 *  - `pending === 'start'`：本窗发起的那一轮（点了连接、start 还没返回）。用户常就在浮层里等着，
 *    取消入口首先要在这里可用；旧实现一律 `disabled={busy}`，等于把取消入口关掉。
 *  - `backendStarting`（后端 `ProxyStatus.starting` 读时投影）：主窗 / 自动连接 / 崩溃自愈发起的那轮。
 *    托盘没有 store 可共享，只能从状态快照得知。**缺了它**：从主窗点连接后再打开托盘，看到的仍是
 *    「连接代理」⇒ 点下去在已有起核腿之上再叠一个核（不是文案问题，是真会多起一个进程）。
 *
 * 托盘不承载错误态（错误经 `proxy:error` 在主窗呈现）→ `hasError` 恒 false。
 */
export function deriveTrayConnectButton(input: {
  /** 核是否在跑（`ProxyStatus.running`）。 */
  running: boolean;
  /** 后端报的「起核腿在飞」（`ProxyStatus.starting`；字段缺省视作 false）。 */
  backendStarting: boolean;
  /** 本窗发起的在飞启停方向。 */
  pending: 'start' | 'stop' | null;
  /** 已配置可启动的出口（含「直连」哨兵，见 `isTrayServerConfigured`）。 */
  serverConfigured: boolean;
}): ConnectButtonState {
  return deriveConnectButtonState({
    proxyPhase: deriveProxyPhase({
      starting: input.pending === 'start' || input.backendStarting,
      stopping: input.pending === 'stop',
    }),
    isConnected: input.running,
    hasError: false,
    isServerConfigured: input.serverConfigured,
  });
}

/**
 * 托盘状态卡的出口 IP：**不跨连接态回落**，与状态栏同一口径
 * （`components/layout/status-bar-display.ts` 第 3 条 `resolveStatusBarExitIp`）。
 *
 * 已连接只认 proxy 腿、未连接只认 direct 腿。旧写法 `proxy?.ip ?? direct?.ip` 会在代理出口尚未探到时
 * 把**本机** IP 当成「出口」显示 —— 与「不得用入口域名派生出口位置」是同一类错误（见 `domain/exit-flag.ts`），
 * 只不过冒充者换成了本机直连腿。
 *
 * **未探到返回空串**（不是状态栏那个 `'—'`）：托盘状态卡以空串表示「整段不渲染」
 * （`{ip ? \`${nodeName} · ${ip}\` : nodeName}`），塞 `'—'` 会渲染成「节点名 · —」这种噪音行。
 *
 * @param connected 核是否在跑
 * @param proxyIp 代理出口探测到的 IP
 * @param directIp 本机直连出口探测到的 IP
 */
export function resolveTrayExitIp(
  connected: boolean,
  proxyIp: string | undefined,
  directIp: string | undefined
): string {
  return (connected ? proxyIp : directIp) ?? '';
}
