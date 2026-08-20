/**
 * `runtimeLevelView` 的判据 —— 守的是三条**不变量**，不是显示细节：
 *
 *  1. 读不到绝不回落成某个具体级别；
 *  2. 任一方向不同都明示待同步，不把真实滞后吞成「无后果」；
 *  3. 成因二分（暂存未应用 ↔ 核没重启）按**盘上值**判，因为两者补救动作不同。
 *
 * 变异锁（逐条实测转红）见文件末尾清单。
 */
import { describe, it, expect } from 'vitest';

import { runtimeLevelView } from './runtime-level';

describe('runtimeLevelView', () => {
  it('首次渲染（还没取回）是 pending，不冒充「读不到」', () => {
    expect(runtimeLevelView(null, 'info', 'info')).toEqual({ kind: 'pending' });
  });

  it('读到了核在跑的级别，且与控件一致 → known 且无分叉', () => {
    expect(runtimeLevelView({ level: 'info', reason: null }, 'info', 'info')).toEqual({
      kind: 'known',
      level: 'info',
      drift: null,
    });
  });

  it('never_falls_back_when_core_not_running：核没跑 → notRunning，不吐出任何级别', () => {
    const v = runtimeLevelView({ level: null, reason: 'notRunning' }, 'debug', 'debug');
    expect(v).toEqual({ kind: 'notRunning' });
    expect(JSON.stringify(v)).not.toContain('debug');
  });

  it('never_falls_back_when_unreachable：读不到 → unavailable，不吐出任何级别', () => {
    const v = runtimeLevelView({ level: null, reason: 'unavailable' }, 'debug', 'debug');
    expect(v).toEqual({ kind: 'unavailable' });
    expect(JSON.stringify(v)).not.toContain('debug');
  });

  /** 后端将来多一种 reason（或字段缺失）也不能变成「编一个级别出来」。 */
  it('未知 reason 一律按 unavailable 呈现', () => {
    const v = runtimeLevelView({ level: null, reason: 'somethingNew' as never }, 'fatal', 'fatal');
    expect(v).toEqual({ kind: 'unavailable' });
  });
});

/**
 * 不变量 2。两个方向都是「控件意图尚未进核」：即便核更啰嗦只会多记行，也不能伪装成已同步。
 */
describe('双向一致性', () => {
  it('核比控件更严（启动诊断会缺行）→ 报', () => {
    expect(runtimeLevelView({ level: 'info', reason: null }, 'debug', 'debug')).toEqual({
      kind: 'known',
      level: 'info',
      drift: 'coreRestart',
    });
  });

  it('核比控件更啰嗦（刚把级别调低、核未重启）→ 仍报待同步', () => {
    expect(runtimeLevelView({ level: 'debug', reason: null }, 'error', 'error')).toEqual({
      kind: 'known',
      level: 'debug',
      drift: 'coreRestart',
    });
  });

  it('隐私锁抬级后解锁看到的那一格（核 warn / 控件 info）走「核更严」这支', () => {
    expect(runtimeLevelView({ level: 'warn', reason: null }, 'info', 'info')).toEqual({
      kind: 'known',
      level: 'warn',
      drift: 'coreRestart',
    });
  });
});

/** 不变量 3：补救动作不同（应用+重启 ↔ 只需重启），故不能合并成一个 boolean。 */
describe('成因分流', () => {
  it('盘上仍是旧值 → unsaved（改动还在暂存区）', () => {
    expect(runtimeLevelView({ level: 'info', reason: null }, 'debug', 'info')).toEqual({
      kind: 'known',
      level: 'info',
      drift: 'unsaved',
    });
  });

  it('盘上已是新值 → coreRestart（落盘了，核没重启）', () => {
    expect(runtimeLevelView({ level: 'info', reason: null }, 'debug', 'debug')).toEqual({
      kind: 'known',
      level: 'info',
      drift: 'coreRestart',
    });
  });

  it('盘上值还没水合 → coreRestart，不猜成 unsaved', () => {
    expect(runtimeLevelView({ level: 'info', reason: null }, 'debug', null)).toEqual({
      kind: 'known',
      level: 'info',
      drift: 'coreRestart',
    });
  });
});

/** 认不出的级别名不得被当成「一致」吞掉 —— 说「不一样」尚可自证，说「一样」是编造。 */
describe('未知级别名', () => {
  it('核跑在本仓五档之外的 trace 且与控件不同 → 照样报待同步', () => {
    expect(runtimeLevelView({ level: 'trace', reason: null }, 'debug', 'debug')).toEqual({
      kind: 'known',
      level: 'trace',
      drift: 'coreRestart',
    });
  });

  it('完全认不出的级别名 → 报分叉，不吞', () => {
    expect(runtimeLevelView({ level: 'verbose', reason: null }, 'info', 'info')).toEqual({
      kind: 'known',
      level: 'verbose',
      drift: 'coreRestart',
    });
  });
});

/*
 * 变异锁（逐条实测转红）：
 *  · `level === null` 那支改成 `{ kind:'known', level: shown, drift:null }` → never_falls_back_* 两条红。
 *  · `driftOf` 对「核更啰嗦」特判返 null →「核比控件更啰嗦」+「trace」两条红。
 *  · `pendingCause` 恒返 'coreRestart' → 「盘上仍是旧值 → unsaved」红。
 *  · `pendingCause` 的 `savedLevel !== null` 去掉（未水合时当成 unsaved）→ 「盘上值还没水合」红。
 *  · 未知级别名那支改成 `return null` → 「完全认不出的级别名」红。
 */
