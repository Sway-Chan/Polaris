/**
 * 测速反馈文案单测（vitest，node 环境）。
 *
 * 钉住的是「诚实性」而非文案本身：后端把「本层测不了」和「测了但失败」分成不同 code，前端必须分流；
 * 缺席节点数必须如实累加，不能让「全部测速」看着像全测过了。
 */

import { describe, expect, it } from 'vitest';
import type { TFunction } from 'i18next';
import { IpcError } from '@/ipc';
import {
  notInPoolMessage,
  speedTestErrorMessage,
  speedTestBlockedMessage,
} from './speedtest-feedback';
import type { ServerConfig } from '@/contracts/types';
import { speedTestBlockReason, type SpeedTestBlockReason } from '../nodes/nodes-logic';

// 直通 t：返回 key，便于断言走了哪个分支（不校验译文内容，那是 locale 的事）。
const t = ((key: string, opts?: unknown) => {
  if (opts && typeof opts === 'object' && 'count' in (opts as Record<string, unknown>)) {
    return `${key}:${(opts as { count: number }).count}`;
  }
  return key;
}) as unknown as TFunction;

describe('speedTestErrorMessage', () => {
  it('无活跃出口 → 专用文案（不是笼统「失败」）', () => {
    const err = new IpcError('server_speed_test', 'backend msg', 'SPEEDTEST_NO_ACTIVE_EXIT');
    expect(speedTestErrorMessage(err, t)).toBe('nodes.speedTestNoActiveExit');
  });

  it('探针池未接线 → 专用文案', () => {
    const err = new IpcError('server_speed_test', 'backend msg', 'SPEEDTEST_PROBE_POOL_UNWIRED');
    expect(speedTestErrorMessage(err, t)).toBe('nodes.speedTestOnlyActive');
  });

  // 真失败必须透出后端原文：换成笼统文案会掩盖真实原因，用户无从下手。
  it('其它 IpcError → 透出后端原始 message', () => {
    const err = new IpcError('server_speed_test', '核未运行', 'SOME_OTHER_CODE');
    expect(speedTestErrorMessage(err, t)).toBe('核未运行');
  });

  it('非 IpcError → 透出 Error.message', () => {
    expect(speedTestErrorMessage(new Error('boom'), t)).toBe('boom');
  });

  it('非 Error 抛出物 → 字符串化，不崩', () => {
    expect(speedTestErrorMessage('plain', t)).toBe('plain');
  });
});

describe('notInPoolMessage', () => {
  it('全部测到 → null（不打扰）', () => {
    expect(notInPoolMessage({ notInPool: [], tsNotReady: [] }, t)).toBeNull();
  });

  it('只有未入池 → 只报「重启内核纳入」那一句', () => {
    expect(notInPoolMessage({ notInPool: ['a', 'b'], tsNotReady: [] }, t)).toBe(
      'nodes.speedTestSkipped:2'
    );
  });

  /**
   * 两类分开报，**不合计**：合计成一条会把「TS 没登录」说成「未入运行核测速池，重启内核后纳入」，
   * 用户照着重启内核，重启完照旧 —— 真正缺的是登录。变异对照：改回
   * `count = notInPool.length + tsNotReady.length` 的单句形态 → 本条与下一条转红。
   */
  it('TS 未登录就绪单独成句（修法是去登录，不是重启内核）', () => {
    expect(notInPoolMessage({ notInPool: [], tsNotReady: ['t1', 't2'] }, t)).toBe(
      'nodes.speedTestSkippedTsNotReady:2'
    );
  });

  it('两类并存 → 两句都报，且各报各的数（不是合计成 3）', () => {
    const msg = notInPoolMessage({ notInPool: ['a', 'b'], tsNotReady: ['c'] }, t);
    expect(msg).toContain('nodes.speedTestSkipped:2');
    expect(msg).toContain('nodes.speedTestSkippedTsNotReady:1');
    expect(msg).not.toContain('nodes.speedTestSkipped:3');
  });

  it('字段缺省 → 按 0 计，不崩', () => {
    expect(notInPoolMessage({} as { notInPool: string[]; tsNotReady: string[] }, t)).toBeNull();
  });
});

/**
 * 不可测原因 → 文案。这层是 Home 的「网络检测」与 Nodes 的灰 ⚡ tooltip **共用**的措辞源，
 * 守的是「每个原因码都有专属说法」——退回兜底的 `nodes.speedTestNotApplicable`
 * （「不支持测速」）等于把「Tailscale 没设出口」和「代理没跑」说成同一句话，用户按错方向折腾。
 */
describe('speedTestBlockedMessage', () => {
  /** 与 `SpeedTestBlockReason` 联合类型逐项对齐；漏一项 → 下面的穷尽断言转红。 */
  const EXPECTED: Record<Exclude<SpeedTestBlockReason, 'other'>, string> = {
    'staged-only': 'nodes.speedTestBlockedStagedOnly',
    'system-interface': 'nodes.speedTestBlockedSystem',
    'ts-no-exit': 'nodes.speedTestBlockedTsNoExit',
    'lan-only': 'nodes.speedTestBlockedLanOnly',
    'ts-core-not-ready': 'nodes.speedTestBlockedTsCoreNotReady',
    'custom-endpoint': 'nodes.speedTestBlockedCustomEndpoint',
  };

  it('每个具体原因码各有专属键，无一退回兜底', () => {
    for (const [reason, key] of Object.entries(EXPECTED)) {
      const msg = speedTestBlockedMessage(reason as SpeedTestBlockReason, t);
      expect(msg, `${reason} 退回了兜底文案`).toBe(key);
      expect(msg).not.toBe('nodes.speedTestNotApplicable');
    }
  });

  it('未知原因 → 兜底「不支持测速」，不抛也不返 undefined', () => {
    expect(speedTestBlockedMessage('other', t)).toBe('nodes.speedTestNotApplicable');
    expect(speedTestBlockedMessage('brand-new-reason' as SpeedTestBlockReason, t)).toBe(
      'nodes.speedTestNotApplicable'
    );
  });

  /**
   * 端到端闭合：`speedTestBlockReason` 真正会产出的每个码，本函数都得给出专属说法。
   * 上面的 EXPECTED 是手写表（可能与谓词分叉），这条用**谓词自己的产物**当输入 ——
   * 谓词新增一个分支而本函数没跟上时，它会退回兜底，这条转红。
   */
  it('谓词实际产出的码全部有专属文案（不是手写表自说自话）', () => {
    const srv = (over: Partial<ServerConfig>): ServerConfig =>
      ({ id: 'x', name: 'x', protocol: 'vless', address: 'a', port: 1, ...over }) as ServerConfig;
    // WG 夹具只填被谓词读到的键；其余必填键（privateKey 等）与本判定无关，故整体断言（同 nodes-logic.test.ts）。
    const cases: Array<[ServerConfig, { mainCorePool: boolean }, boolean]> = [
      [
        srv({ protocol: 'wireguard', wireguardSettings: { reverseMesh: true } } as Partial<ServerConfig>),
        { mainCorePool: true },
        false,
      ],
      [srv({ protocol: 'tailscale', tailscaleSettings: {} }), { mainCorePool: true }, false],
      [
        srv({
          protocol: 'wireguard',
          wireguardSettings: { allowInternet: false, allowedIPs: ['10.0.0.0/8'] },
        } as Partial<ServerConfig>),
        { mainCorePool: true },
        false,
      ],
      [srv({ protocol: 'tailscale', tailscaleSettings: { exitNode: 'n' } }), { mainCorePool: false }, false],
      [srv({ protocol: 'custom', customSettings: { isEndpoint: true, outbound: {} } }), { mainCorePool: true }, false],
      [srv({}), { mainCorePool: true }, true],
    ];
    for (const [server, caps, stagedOnly] of cases) {
      const reason = speedTestBlockReason(server, caps, stagedOnly);
      expect(reason, '构造的用例应当是不可测的').not.toBeNull();
      expect(
        speedTestBlockedMessage(reason as SpeedTestBlockReason, t),
        `原因码 ${reason} 没有专属文案`
      ).not.toBe('nodes.speedTestNotApplicable');
    }
  });
});
