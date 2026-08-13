import { describe, expect, it } from 'vitest';
import {
  autoMtuFor,
  defaultMtuFor,
  parseMtuInput,
  platformDefaultStack,
  resolveTunStack,
  MTU_MAX,
  MTU_MIN,
} from './tun-mtu';

/// 🔴 **本表与 Rust 侧 `tun_stack.rs::default_mtu_by_stack_and_platform` 逐格对应**。
///
/// 生成期真值在 Rust；本文件只影响设置项显示的「自动 = 多少」。但两侧一旦分叉，用户看到的
/// 自动值就不是内核实际拿到的值 —— 那比不显示更糟。任一侧改值而另一侧没跟，两边各红一处。
describe('TUN 默认 MTU（栈 × 平台）', () => {
  it('gvisor + Windows → 65535（实测全矩阵最高格 919 Mbps）', () => {
    expect(defaultMtuFor('gvisor', 'win')).toBe(65535);
  });

  /// mac/linux 的吞吐实验都撞了别的瓶颈（1GbE 线速 / Wi-Fi 噪声 / CPU 争抢），**没有区分力**。
  /// 故不照搬 Windows 的 65535 —— 没有数据支持的极端值不该当默认。
  it('gvisor + mac/linux → 9000，不照搬 Windows 的极端值', () => {
    expect(defaultMtuFor('gvisor', 'mac')).toBe(9000);
    expect(defaultMtuFor('gvisor', 'lin')).toBe(9000);
    expect(defaultMtuFor('gvisor', undefined)).toBe(9000);
  });

  /// 下界由正确性钉死：system + 1350 会丢 1400B UDP 数据报（Windows/Linux/macOS 三平台各自复现
  /// 0/5，同 MTU 下 gvisor 均通过）。上界由 65535 塌到 11 Mbps 钉死。4064 同时避开两头。
  it('system/mixed 三平台同为 4064', () => {
    for (const p of ['win', 'mac', 'lin'] as const) {
      expect(defaultMtuFor('system', p)).toBe(4064);
      expect(defaultMtuFor('mixed', p)).toBe(4064);
    }
  });
});

describe('栈解析', () => {
  /// 🔴 Windows auto → gvisor（2026-08-05 起，此前是 system）。
  it('auto 跟随平台：mac·win→gvisor / linux→system', () => {
    expect(platformDefaultStack('win')).toBe('gvisor');
    expect(platformDefaultStack('mac')).toBe('gvisor');
    expect(platformDefaultStack('lin')).toBe('system');
    expect(platformDefaultStack(undefined)).toBe('system');
  });

  it('显式选择全平台 honor，零强制回退', () => {
    expect(resolveTunStack('system', 'mac')).toBe('system');
    expect(resolveTunStack('gvisor', 'lin')).toBe('gvisor');
    expect(resolveTunStack('mixed', 'win')).toBe('mixed');
  });

  it('undefined 与 auto 等价', () => {
    expect(resolveTunStack(undefined, 'win')).toBe('gvisor');
    expect(resolveTunStack('auto', 'win')).toBe('gvisor');
  });

  /// 设置项占位符读的就是这个：显式选栈时自动值随之变（Win 选 system → 4064，不再是 65535）。
  it('自动值随显式选栈改变', () => {
    expect(autoMtuFor('auto', 'win')).toBe(65535);
    expect(autoMtuFor('system', 'win')).toBe(4064);
    expect(autoMtuFor('mixed', 'win')).toBe(4064);
    expect(autoMtuFor('auto', 'mac')).toBe(9000);
  });
});

describe('MTU 输入解析', () => {
  it('空白 → 自动（mtu 缺席）', () => {
    expect(parseMtuInput('')).toEqual({ mtu: undefined });
    expect(parseMtuInput('   ')).toEqual({ mtu: undefined });
  });

  it('区间内整数原样收下', () => {
    expect(parseMtuInput('4064')).toEqual({ mtu: 4064 });
    expect(parseMtuInput(String(MTU_MIN))).toEqual({ mtu: MTU_MIN });
    expect(parseMtuInput(String(MTU_MAX))).toEqual({ mtu: MTU_MAX });
  });

  /// 🔴 越界**不钳制**。悄悄把 70000 改成 65535 = 框里是用户填的数、生效的是另一个，
  /// 与旧实现「填 9000 被静默改写成 1350」是同一类缺陷。宁可当场报错。
  it('越界与非数字一律判非法，不做钳制', () => {
    expect(parseMtuInput('70000')).toEqual({ invalid: true });
    expect(parseMtuInput('1279')).toEqual({ invalid: true });
    expect(parseMtuInput('0')).toEqual({ invalid: true });
    expect(parseMtuInput('abc')).toEqual({ invalid: true });
    expect(parseMtuInput('4064.5')).toEqual({ invalid: true });
    expect(parseMtuInput('-4064')).toEqual({ invalid: true });
  });
});
