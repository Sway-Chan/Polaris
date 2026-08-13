/**
 * StatusBar 展示派生的纯逻辑（供 StatusBar.tsx 消费 + vitest 直测）。
 *
 * 抽成独立模块而非内联在组件里：vitest 走 node 环境（vite.config.ts 未引 jsdom），StatusBar
 * 组件本身订了 api.ipInfo/api.server/api.stats 测不了；三条口径都是本轮修的语义偏离，
 * 值得被单测钉住（同 node-edit-routing.ts / speedtest-feedback.ts 先例）。
 *
 * 三条口径对齐原型（行号见 prototype/polaris-prototype.html）/ 契约（polaris-上游-capability-contract.md）：
 *  1. 节点名不随连接态切换——原型 `setConnected()`（:3433-3455）断开分支不碰 `#sb-node`，
 *     断开态残留显示上次节点名，而非「未连接」（那已由左侧状态点+文案表达，重复徒增噪音）；
 *     直连哨兵命中显「直连」（:3513 `pickDirectExit` 设 `#sb-node`=Direct），不看连接态。
 *  2. 延迟文本仅连接态显示实测值，断开恒 '—'（:3502 `$('#sb-lat').textContent= st.connected? latText(n.lat) : '—'`；
 *     :3450 `setConnected(false)` 同样把 `#sb-lat` 置 '—'）。
 *  3. 出口 IP 按连接态分叉、不跨态回落（契约「状态栏」条目：连→代理出口 IP / 未连→本地出口 IP）；
 *     此前 `ipInfo?.proxy?.ip ?? ipInfo?.direct?.ip` 无视连接态，代理探测未就绪时会把本地 IP
 *     误当「出口」展示。
 */

/**
 * 状态栏出口名。
 *
 * `blockSelected` 是后加的第四参而非塞进 `directSelected`：两者是**并列的哨兵出口**，
 * 合并会让状态栏把「阻断」显示成「直连」——那是最坏的一种谎报（用户以为流量在走，实际全丢）。
 * 漏掉它同样是谎报：`serverName` 查不到（哨兵不是节点 id）⇒ 落到 placeholder「请配置服务器」，
 * 而用户明明选了一个有效出口。
 */
export function resolveStatusBarNodeName(
  directSelected: boolean,
  serverName: string | undefined,
  directLabel: string,
  placeholderLabel: string,
  blockSelected = false,
  blockLabel = ''
): string {
  if (directSelected) return directLabel;
  if (blockSelected) return blockLabel;
  return serverName ?? placeholderLabel;
}

export function resolveStatusBarLatencyText(
  connected: boolean,
  latencyMs: number | null | undefined
): string {
  if (!connected || latencyMs === null || latencyMs === undefined) return '—';
  return `${latencyMs} ms`;
}

export function resolveStatusBarExitIp(
  connected: boolean,
  proxyIp: string | undefined,
  directIp: string | undefined
): string {
  return connected ? (proxyIp ?? '—') : (directIp ?? '—');
}
