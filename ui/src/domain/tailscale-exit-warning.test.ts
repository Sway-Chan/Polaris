/**
 * TS 出口告警谓词矩阵 + **喂数接线**守卫。
 *
 * # 为什么这道门必须同时守「谓词」和「接线」
 *
 * 被修的缺陷不在谓词里 —— `deriveTsExitWarning` 的三条 peer 分支早就写好且注释详尽，但
 * `store.tailscalePeers` **全仓零 setter**（声明了、初始化了、reset 里也清了，就是没人写），于是
 * `peers` 恒为 `undefined` ⇒ `exit-device-offline` / `exit-device-not-advertised` 两条在产品里
 * **永不可达**。只测纯函数会全绿，用户看到的却是「出口设备在线但没广告出口 → 流量出不去 → 界面零提示」。
 *
 * 故本文件两段：
 *  - **P 段**（纯函数矩阵）：判定本身，含新增的 `needs-auth`；
 *  - **W 段**（源码接线不变量，沿用 `store/system-proxy-live-wiring.test.ts` 的模式）：钉住
 *    「STATUS 帧真的被写进 store」「组件真的把它喂进谓词」这两跳，任一跳断掉即转红。
 *    断言跑在**剥掉注释**的源码上（本文件与被守文件的注释都逐字引用了旧形态，扫原文会自我误伤）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { deriveTsExitWarning } from './tailscale-exit-warning';
import type { TsExitWarning } from './tailscale-exit-warning';
import type { TailscaleStatusEvent, TailscaleStatusPeer } from '../contracts/tailscale-status';
import type { ServerConfig } from '../contracts/types';

const SRC = resolve(__dirname, '..');
const read = (rel: string): string => readFileSync(resolve(SRC, rel), 'utf8');
/** 去注释：`[^:]` 前瞻避免把 `https://` 当行注释切掉。 */
const code = (s: string): string =>
  s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const tsServer = (exitNode?: string): ServerConfig =>
  ({
    id: 'ts1',
    name: 'ts',
    protocol: 'tailscale',
    tailscaleSettings: exitNode ? { exitNode } : {},
  }) as unknown as ServerConfig;

const peer = (
  hostName: string,
  ip: string,
  online: boolean,
  exitNodeOption: boolean
): TailscaleStatusPeer => ({
  hostName,
  ip,
  online,
  exitNode: false,
  exitNodeOption,
  active: false,
});

const frame = (
  over: Partial<TailscaleStatusEvent> = {},
  peers: TailscaleStatusPeer[] = []
): TailscaleStatusEvent => ({
  serverId: 'ts1',
  backendState: 'Running',
  loggedIn: true,
  tailscaleIPs: ['100.64.0.9'],
  // Taildrop 四位在本用例无关，取「无能力、无文件」中性值。不给可选/默认是刻意的：
  // 契约加字段时这些夹具必须被人重新看一眼，而不是被 `?:` 静默补齐。
  canShareFiles: false,
  waitingFileCount: 0,
  receivingFileCount: 0,
  unreadFileCount: 0,
  expired: false,
  peers,
  ...over,
});

/** 默认「一切正常」的入参；每条用例只改自己那一格。 */
const at = (over: Partial<Parameters<typeof deriveTsExitWarning>[0]> = {}): TsExitWarning =>
  deriveTsExitWarning({
    selectedServer: tsServer('exit-host'),
    loggedIn: true,
    proxyModeDirect: false,
    proxyRunning: true,
    status: frame({}, [peer('exit-host', '100.64.0.5', true, true)]),
    ...over,
  });

describe('P1：抑制路径（永不误报）', () => {
  it('未选中 / 非 TS / 直连 → none', () => {
    expect(at({ selectedServer: undefined })).toBe('none');
    expect(at({ selectedServer: { ...tsServer('x'), protocol: 'vless' } as ServerConfig })).toBe(
      'none'
    );
    expect(at({ proxyModeDirect: true })).toBe('none');
  });

  it('出口设备在线且已广告 → none（阴性对照：矩阵不是恒报）', () => {
    expect(at()).toBe('none');
  });

  it('exit_node 是匹配不上任何 peer 的自定义值 → none（不误报）', () => {
    expect(
      at({
        selectedServer: tsServer('100.99.99.99'),
        status: frame({}, [peer('other', '100.64.0.9', true, true)]),
      })
    ).toBe('none');
  });
});

describe('P2：needs-auth —— 判据是控制面终局否定，不是超时猜测', () => {
  it.each([
    ['NeedsLogin', false],
    ['NeedsMachineAuth', false],
  ])('backendState=%s → needs-auth', (backendState) => {
    expect(at({ loggedIn: false, status: frame({ backendState, loggedIn: false }) })).toBe(
      'needs-auth'
    );
  });

  it('key 过期（backendState 仍 Running）→ needs-auth', () => {
    expect(
      at({ loggedIn: false, status: frame({ loggedIn: false, expired: true }) })
    ).toBe('needs-auth');
  });

  it.each(['NoState', 'Starting', 'Stopped', '未来的未知态'])(
    '启动过渡帧 backendState=%s → none（「还没启完」不是「凭据无效」，不许在正常连接过程中闪现）',
    (backendState) => {
      expect(at({ loggedIn: false, status: frame({ backendState, loggedIn: false }) })).toBe('none');
    }
  );

  it('无帧 → none（不知道就不猜；这正是「不靠超时」的形态）', () => {
    expect(at({ loggedIn: false, status: undefined })).toBe('none');
  });

  it('核没跑 → none（帧陈旧：核停后用户在浏览器里补完的登录我们收不到，据陈旧帧报错=误报）', () => {
    expect(
      at({
        loggedIn: false,
        proxyRunning: false,
        status: frame({ backendState: 'NeedsLogin', loggedIn: false }),
      })
    ).toBe('none');
  });

  it('折叠登录态说「已登录」但末帧是 NeedsLogin → 仍报 needs-auth（末帧优先于折叠态）', () => {
    // 真机形态：applyTailscaleStateExists 只看 state 目录存在性，会把 NeedsLogin 的节点盖回
    // loggedIn=true。只看折叠态就漏判。
    expect(at({ loggedIn: true, status: frame({ backendState: 'NeedsLogin', loggedIn: false }) })).toBe(
      'needs-auth'
    );
  });

  it('needs-auth 压过「未选出口设备」（根因先行，不指错方向）', () => {
    expect(
      at({
        selectedServer: tsServer(),
        loggedIn: false,
        status: frame({ backendState: 'NeedsLogin', loggedIn: false }),
      })
    ).toBe('needs-auth');
  });
});

describe('P3：出口设备三态', () => {
  it('未配 exit_node → no-exit-device（断开态也报，配置级问题）', () => {
    expect(at({ selectedServer: tsServer() })).toBe('no-exit-device');
    expect(at({ selectedServer: tsServer(), proxyRunning: false })).toBe('no-exit-device');
  });

  it('exit 设备离线 → exit-device-offline', () => {
    expect(at({ status: frame({}, [peer('exit-host', '100.64.0.5', false, true)]) })).toBe(
      'exit-device-offline'
    );
  });

  it('exit 设备在线但未广告出口 → exit-device-not-advertised（上游 点名的漏判格）', () => {
    expect(at({ status: frame({}, [peer('exit-host', '100.64.0.5', true, false)]) })).toBe(
      'exit-device-not-advertised'
    );
  });

  it('同时离线且未广告 → 离线优先（离线态 exitNodeOption 可能陈旧）', () => {
    expect(at({ status: frame({}, [peer('exit-host', '100.64.0.5', false, false)]) })).toBe(
      'exit-device-offline'
    );
  });

  it('exit_node 按 ip 匹配也成立（ip / hostName 双口径）', () => {
    expect(
      at({
        selectedServer: tsServer('100.64.0.5'),
        status: frame({}, [peer('exit-host', '100.64.0.5', true, false)]),
      })
    ).toBe('exit-device-not-advertised');
  });

  it('核没跑 → 不据陈旧 peers 报离线/未广告', () => {
    expect(
      at({ proxyRunning: false, status: frame({}, [peer('exit-host', '100.64.0.5', false, false)]) })
    ).toBe('none');
  });
});

describe('W：喂数接线（谓词再准，没人喂数据也是零提示）', () => {
  const APP = 'App.tsx';
  const STORE = 'store/app-store.ts';
  const VIEW = 'components/screens/home/TsExitWarning.tsx';

  it('STATUS 订阅把**整帧**落 store，而不是只取 loggedIn', () => {
    const src = code(read(APP));
    expect(src).toMatch(/onTailscaleStatus\(\(data\)\s*=>\s*\{[\s\S]{0,200}setTailscaleStatus\(data\)/);
  });

  it('落帧**不得**被登录判决门挡住（那道门是给折叠登录态用的，告警要的是原始帧）', () => {
    // 形态：`setTailscaleStatus(data)` 必须出现在 `if (!isDefinitiveTsLoginFrame(data)) return;` 之前。
    const src = code(read(APP));
    const store = src.indexOf('setTailscaleStatus(data)');
    const gate = src.indexOf('isDefinitiveTsLoginFrame(data)');
    expect(store).toBeGreaterThan(-1);
    expect(gate).toBeGreaterThan(-1);
    expect(store).toBeLessThan(gate);
  });

  it('store 有 tailscaleStatuses 切片与其 setter（此前的 tailscalePeers 是零 setter 死切片）', () => {
    const src = code(read(STORE));
    expect(src).toMatch(/tailscaleStatuses:\s*Record<string,\s*TailscaleStatusEvent\s*\|\s*undefined>/);
    expect(src).toMatch(/setTailscaleStatus:\s*\(event\)\s*=>/);
    expect(src).toMatch(/set\(\{\s*tailscaleStatuses:/);
    // 死切片形态不得复活。
    expect(src).not.toContain('tailscalePeers');
  });

  it('首页组件从 store 读末帧并**真的喂进**谓词（拿到不用等于没接）', () => {
    const src = code(read(VIEW));
    expect(src).toMatch(/s\.tailscaleStatuses\[tsId\]/);
    // 必须是简写 `status,` 或 `status: status` —— 只匹配 `\bstatus\b` 会被 `status: undefined`
    // 骗过（实测：该变异一度存活。「读到了」和「喂进去了」是两件事，字面量占位正是二者之间的裂缝）。
    expect(src).toMatch(/deriveTsExitWarning\(\{[^}]*\bstatus(,|\s*:\s*status\b)/s);
  });

  it('needs-auth 有自己的文案与动作（否则新态会静默复用「选择出口设备」这个错方向的 CTA）', () => {
    const src = code(read(VIEW));
    expect(src).toContain('home.tsExitNeedsAuthWarn');
    expect(src).toContain('home.tsExitGoAuth');
    expect(src).toMatch(/openExternal\(/);
  });
});
