/**
 * 「全量测速」目标集选取回归测（`speedTestableIds`）—— 钉死「主窗口只测速当前节点」那条真机指控。
 *
 * 根因是 UI 层：`HomeScreen` 曾硬编码 `speedTest([currentServer.id])`，而后端分波批量编排一直是好的。
 * 对齐 上游 `connection-control-card.tsx:156` 的 `useSpeedTest(servers)` → `use-speed-test.ts:48-50`。
 *
 * 本文件锁两件事：
 *  1. 集合是**全量节点**（不是当前节点、不是当前订阅、不是可见列表）；
 *  2. 过滤口径是 `isSpeedTestable`（**不是**无脑全测）—— 结构性测不出真值的节点必须排除，
 *     否则会产出假数值（reverseMesh 走 OS default 测出直连假好值、TS-mesh-only 是公网黑洞必假超时）。
 */

import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { speedTestableIds } from './endpoint-routes';
import type { ServerConfig } from '@/contracts/types';

const srv = (id: string, extra: Partial<ServerConfig> = {}): ServerConfig =>
  ({ id, name: id, protocol: 'vmess', server: 'example.com', port: 443, ...extra }) as ServerConfig;

describe('speedTestableIds：全量而非单节点', () => {
  it('普通节点全部纳入（**保序**，结果按请求序流式回填）', () => {
    const servers = [srv('a'), srv('b'), srv('c')];
    expect(speedTestableIds(servers)).toEqual(['a', 'b', 'c']);
  });

  it('不是「只测当前出口」：集合与 selectedServerId 无关（本条即那份真机指控的回归锁）', () => {
    const servers = [srv('a'), srv('b'), srv('c')];
    // 无论当前选中谁，全量集合恒为三个 —— 函数签名里压根没有 selectedServerId 的位置。
    expect(speedTestableIds(servers)).toHaveLength(3);
  });

  it('空节点列表 → 空集（调用方据此提示而非空跑）', () => {
    expect(speedTestableIds([])).toEqual([]);
  });
});

describe('speedTestableIds：过滤口径 = isSpeedTestable（不产假数值）', () => {
  it('reverseMesh（system 内核接口）排除 —— 否则 dial 走 OS default 测出直连假好值', () => {
    const servers = [srv('ok'), srv('rev', { protocol: 'wireguard', wireguardSettings: { reverseMesh: true } } as Partial<ServerConfig>)];
    expect(speedTestableIds(servers)).toEqual(['ok']);
  });

  it('custom endpoint 排除（raw-JSON 无 gate 真值）', () => {
    const servers = [srv('ok'), srv('cus', { protocol: 'custom', customSettings: { isEndpoint: true } } as Partial<ServerConfig>)];
    expect(speedTestableIds(servers)).toEqual(['ok']);
  });

  it('TS-exit 是 **path-aware**：主核池不可用（代理没跑）时排除，可用时纳入', () => {
    const servers = [srv('ok'), srv('ts', { protocol: 'tailscale', tailscaleSettings: { exitNode: 'node-x' } } as Partial<ServerConfig>)];

    // 代理未运行 → 主核池不可用 → TS-exit 不可测（临时核建不出第二 tsnet 实例）
    expect(speedTestableIds(servers, { mainCorePool: false })).toEqual(['ok']);
    // 代理在跑 → 池可用 → 纳入
    expect(speedTestableIds(servers, { mainCorePool: true })).toEqual(['ok', 'ts']);
  });

  it('TS-mesh-only（无 exitNode）恒排除：公网黑洞必假超时，池可用也不测', () => {
    const servers = [srv('ok'), srv('tsm', { protocol: 'tailscale' } as Partial<ServerConfig>)];
    expect(speedTestableIds(servers, { mainCorePool: true })).toEqual(['ok']);
  });

  it('普通 WG mesh-only（allowInternet=off）同样排除 —— 与 TS-mesh-only 对称', () => {
    // 关了「允许访问外网」的组网节点，peer.allowed_ips 只含具体段（wireguardPeerAllowedIps），
    // 公网探测 URL 不命中 cryptokey routing 即被丢弃 → 必返 -1，而 -1 在 UI 上读作「真实超时」= 假数值。
    const servers = [
      srv('ok'),
      srv('wgLan', {
        protocol: 'wireguard',
        wireguardSettings: { allowInternet: false },
      } as Partial<ServerConfig>),
    ];
    expect(speedTestableIds(servers, { mainCorePool: true })).toEqual(['ok']);
    // 反向：默认/显式开启外网的 WG 仍可测（别把整族 WireGuard 一起误排）。
    const on = [
      srv('wgDefault', { protocol: 'wireguard', wireguardSettings: {} } as Partial<ServerConfig>),
      srv('wgOn', {
        protocol: 'wireguard',
        wireguardSettings: { allowInternet: true },
      } as Partial<ServerConfig>),
    ];
    expect(speedTestableIds(on)).toEqual(['wgDefault', 'wgOn']);
  });

  it('全部不可测 → 空集（调用方提示「无可测节点」，不发空请求）', () => {
    const servers = [srv('tsm', { protocol: 'tailscale' } as Partial<ServerConfig>)];
    expect(speedTestableIds(servers, { mainCorePool: true })).toEqual([]);
  });
});

/* ────────────────────────────────────────────────────────────────────────────
 * 消费面守卫 —— 首页两条测速腿必须同口径、共用 testing 标志
 *
 * 上面那组只证明 `speedTestableIds` 本身对，证不了**首页真的在两条腿上都用它**。本仓 vitest 是
 * node 环境（无 jsdom/testing-library），`HomeScreen` 渲染不了 ⇒ 出口选单「全部测速」那条腿绕开
 * 过滤/绕开 `testing` 时，上面全绿而缺陷照旧（射程 ≠ 批次范围）。故补一条扫源码的结构守卫。
 *
 * 守的两件事（复审 #8 的两条子指控）：
 *  ① 菜单腿必须过 `speedTestableIds` —— 否则会请求结构上不可测的节点（reverseMesh / custom endpoint /
 *     TS-mesh-only），它们返 `-1`，而 `-1` 在 UI 上读作「真实超时」而非「未测」= 伪造数值；
 *  ② 菜单腿必须与圆钮**共用** `testing` —— 否则批量进行中主测速按钮仍可点，撞后端单飞闸返
 *     `CODE_IN_FLIGHT` + 弹错误 toast（用户读作「测速失败」，实为自己撞自己）。
 * ──────────────────────────────────────────────────────────────────────────── */

describe('消费面守卫：首页菜单「全部测速」与圆钮同口径', () => {
  /** 取顶层 `const <name> = useCallback(` 到其收尾 `);`（列 2 缩进）为止的函数体。 */
  function callbackBody(src: string, name: string): string {
    const anchor = `const ${name} = useCallback(`;
    const start = src.indexOf(anchor);
    expect(start, `锚点消失，守卫已失去判据: ${anchor}`).toBeGreaterThan(-1);
    const rest = src.slice(start);
    const end = rest.indexOf('\n  );');
    expect(end, `找不到 ${name} 的 useCallback 收尾`).toBeGreaterThan(-1);
    return rest.slice(0, end);
  }

  const home = readFileSync(
    fileURLToPath(new URL('../components/screens/home/HomeScreen.tsx', import.meta.url)),
    'utf8'
  );

  it('菜单腿 onTestAllInMenu 经 speedTestableIds 过滤后才请求（不测结构上不可测的节点）', () => {
    const body = callbackBody(home, 'onTestAllInMenu');
    expect(body, '菜单腿必须与圆钮同口径过滤，否则不可测节点返 -1 会被读成真实超时').toContain(
      'speedTestableIds('
    );
    expect(
      body.includes('api.server.speedTest(target)'),
      '必须请求**过滤后**的集合（target），而不是菜单直接给的可见 id 全集'
    ).toBe(true);
  });

  it('菜单腿与圆钮共用 testing 标志（批量进行中主按钮不得可点）', () => {
    const body = callbackBody(home, 'onTestAllInMenu');
    expect(body, '菜单腿必须置 testing，否则主测速按钮在批量期间仍可点 → CODE_IN_FLIGHT').toContain(
      'setTesting(true)'
    );
    expect(body, 'finally 里必须复位 testing，否则按钮永久灰到组件重挂载').toContain(
      'setTesting(false)'
    );
  });

  /**
   * 圆钮腿（「网络检测」）**2026-07-31 起只测当前出口**，故不再与菜单腿共用 `speedTestableIds`
   * 那一条（射程判据见 `components/screens/home/home-speedtest-scope.test.ts`）。
   * 但**单飞标志这条对称必须留着**：两条腿共用 `testing`，否则一条在跑时另一条仍可点，
   * 第二次请求撞后端进程级单飞闸返 `CODE_IN_FLIGHT` —— 用户看见的是「测速失败」，实为自己撞自己。
   */
  it('圆钮腿 onSpeedTest 与菜单腿共用 testing 单飞标志（射程可以不同，单飞不能不同）', () => {
    const body = callbackBody(home, 'onSpeedTest');
    expect(body).toContain('setTesting(true)');
    expect(body).toContain('setTesting(false)');
  });
});
