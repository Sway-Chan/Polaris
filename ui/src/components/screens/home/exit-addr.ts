/**
 * 首页出口卡那一行「连接信息」的取值 —— 纯逻辑，可离线单测。
 *
 * # 为什么需要它（真机实证 2026-07-31）
 *
 * 原实现是 `HomeScreen` 里一句无保护的模板串 `` `${server.address}:${server.port}` ``。
 * 而 **endpoint 类节点（Tailscale / WARP / WireGuard）的 `address`/`port` 合法为空** ——
 * 这正是那日 blocker 的另一面：`ServerConfig` 此前对它们做协议盲的必填校验，一个 TS 节点
 * 就能让整份配置反序列化失败（见 `crates/config-engine/.../server_config.rs` 的 `#[serde(default)]`）。
 * 放开必填之后，卡片那句模板串就把两个缺席字段直接插了进去，用户看到的是字面的
 * `undefined:undefined`。
 *
 * # 取值顺序
 *
 * 1. `address` 非空 → `address:port`（常规代理节点，原行为逐字不变；`port` 缺席则不拼冒号）；
 * 2. Tailscale 节点 → **设定的出口设备**（`tailscaleSettings.exitNode`，name 或 IP 皆可）；
 * 3. 都没有 → `null`，由调用方渲染占位符。
 *
 * ## 为什么第 2 条读配置而不是读实时状态帧（2026-08-03 订正）
 *
 * 本函数一度用 STATUS 帧里的 **tailnet 自身 IP**（`self.tailscaleIPs`）作 TS 节点的兜底。
 * 上机证伪：tsnet 卡在 `NoState` 时该帧恒为空 ⇒ 卡片恒显 `—`，而那恰恰是用户最想知道
 * 「我这条 TS 到底连去哪」的时刻。而**出口设备是静态配置，永远读得到**，与核跑不跑无关。
 *
 * 顺带也更贴语义：出口卡问的是「出网走哪」，答案就是设定的出口设备；tailnet 自身 IP 回答的是
 * 「我在 tailnet 里叫什么」，是另一个问题（那个归组网信息卡）。
 *
 * **绝不返回含 `undefined` 的串**：那是本函数存在的全部理由。
 */

import type { ServerConfig } from '@/contracts/types';

/** 出口卡取值所需的最小字段面（便于单测构造，也标明本函数只读这三处）。 */
type ExitAddrSource = Pick<ServerConfig, 'address' | 'port' | 'tailscaleSettings'>;

/**
 * 出口卡的连接信息。`null` = 无可显示（调用方渲染 `—`）。
 */
export function exitAddrText(server: ExitAddrSource | null | undefined): string | null {
  if (server === null || server === undefined) return null;
  const addr = (server.address ?? '').trim();
  if (addr !== '') {
    // port 缺席时不拼冒号 —— 拼了就是 `1.2.3.4:undefined`，同一类缺陷的另一半。
    return server.port ? `${addr}:${server.port}` : addr;
  }
  const exitNode = (server.tailscaleSettings?.exitNode ?? '').trim();
  return exitNode !== '' ? exitNode : null;
}
