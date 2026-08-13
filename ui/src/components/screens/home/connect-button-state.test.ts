import { describe, it, expect } from 'vitest';
import {
  deriveConnectButtonState,
  connectButtonClass,
  deriveProxyPhase,
} from './connect-button-state';

const base = {
  proxyPhase: 'idle' as const,
  isConnected: false,
  hasError: false,
  isServerConfigured: true,
};

describe('deriveConnectButtonState', () => {
  it('未连接 + 已配置 → start，可点', () => {
    expect(deriveConnectButtonState(base)).toEqual({
      kind: 'start',
      busy: false,
      disabled: false,
      action: 'start',
    });
  });

  it('未配置节点 → start 但 disabled（title 给原因）', () => {
    expect(deriveConnectButtonState({ ...base, isServerConfigured: false })).toEqual({
      kind: 'start',
      busy: false,
      disabled: true,
      action: 'start',
    });
  });

  it('已连接 → stop，恒可点（断开不受配置完整性约束）', () => {
    expect(
      deriveConnectButtonState({ ...base, isConnected: true, isServerConfigured: false })
    ).toEqual({ kind: 'stop', busy: false, disabled: false, action: 'stop' });
  });

  it('未连接 + 有错误 → error，可点重试', () => {
    expect(deriveConnectButtonState({ ...base, hasError: true })).toEqual({
      kind: 'error',
      busy: false,
      disabled: false,
      action: 'start',
    });
  });

  it('有错误 + 未配置节点 → error 且 disabled（重试也无从连起）', () => {
    expect(
      deriveConnectButtonState({ ...base, hasError: true, isServerConfigured: false })
    ).toEqual({ kind: 'error', busy: false, disabled: true, action: 'start' });
  });

  // 回归：残留 error + 核已运行时按钮须显 stop，否则显 error 却执行停止，自相矛盾
  it('已连接优先于 error → stop', () => {
    expect(deriveConnectButtonState({ ...base, isConnected: true, hasError: true })).toEqual({
      kind: 'stop',
      busy: false,
      disabled: false,
      action: 'stop',
    });
  });

  /**
   * **对 上游 `connect-button-state.ts:28` 刻意偏离的门**（用户授权，勿"修"回去）。
   *
   * 上游 原版 `starting → { busy:true, disabled:true }`，本仓移植时逐字照搬。真机事故：TUN 起核
   * 连续 FATAL、预算内重试 ≈35s，全程 running:false ⇒ 圆钮停在 starting 且点不动，用户原话
   * 「甚至启动卡死阶段无法关闭启动过程」。后端一直可取消，缺的只是这个入口。
   *
   * 变异（任一转红）：`disabled` 改回 true → 取消入口又没了；`action` 改回 'start' → 启动中点击
   * 变成再叠一次起核。
   */
  it('starting → busy 但**可点 = 取消**（对 上游 :28 的刻意偏离）', () => {
    expect(
      deriveConnectButtonState({
        proxyPhase: 'starting',
        isConnected: true,
        hasError: true,
        isServerConfigured: false,
      })
    ).toEqual({ kind: 'starting', busy: true, disabled: false, action: 'cancel' });
  });

  it('starting 的取消入口不受「未配置节点」约束（核都在起了，配置不该妨碍叫停）', () => {
    const s = deriveConnectButtonState({ ...base, proxyPhase: 'starting', isServerConfigured: false });
    expect(s.disabled).toBe(false);
    expect(s.action).toBe('cancel');
  });

  // stopping 保持 上游 语义：停止已是终态意图，"取消停止"= 重新启动，属另一个意图。
  it('stopping 相位压过一切 → busy + disabled + 无可操作', () => {
    expect(
      deriveConnectButtonState({
        proxyPhase: 'stopping',
        isConnected: true,
        hasError: true,
        isServerConfigured: false,
      })
    ).toEqual({ kind: 'stopping', busy: true, disabled: true, action: 'none' });
  });

  /**
   * 消费方**只许按 action 分发**的门。起核期 `isConnected` 恒 false —— 谁再按它猜一次，谁就会
   * 在启动中走进 start 分支（= `TrayMenu.tsx` 原 :219-236 的缺陷：叠第二次起核）。
   */
  it('starting 期 isConnected 为 false，但 action 必须是 cancel（禁止按 isConnected 猜）', () => {
    const s = deriveConnectButtonState({ ...base, proxyPhase: 'starting', isConnected: false });
    expect(s.action).toBe('cancel');
    expect(s.action).not.toBe('start');
  });
});

describe('deriveProxyPhase', () => {
  it('idle', () => {
    expect(deriveProxyPhase({ starting: false, stopping: false })).toBe('idle');
  });

  it('只在启动 → starting', () => {
    expect(deriveProxyPhase({ starting: true, stopping: false })).toBe('starting');
  });

  it('只在停止 → stopping', () => {
    expect(deriveProxyPhase({ starting: false, stopping: true })).toBe('stopping');
  });

  /**
   * **取消途中两标志同时为真**（start 还在飞、stop 已发出）。stopping 必须压过 starting，
   * 否则圆钮在取消途中仍显"可点取消" → 用户重复点、每点一次多发一条 stop。
   * 变异：把两条判定顺序对调 → 本测转红。
   */
  it('取消途中（两者同真）→ stopping 压过 starting', () => {
    expect(deriveProxyPhase({ starting: true, stopping: true })).toBe('stopping');
  });
});

describe('connectButtonClass', () => {
  it.each([
    ['starting', 'busy'],
    ['stopping', 'busy'],
    ['stop', 'on'],
    ['error', 'err'],
    ['start', 'off'],
  ] as const)('%s → .%s', (kind, cls) => {
    expect(connectButtonClass(kind)).toBe(cls);
  });
});
