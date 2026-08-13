import { describe, expect, it } from 'vitest';
import { exitAddrText } from './exit-addr';

describe('出口卡连接信息', () => {
  /// 🔴 **本条是那轮缺陷的回归锁**：endpoint 节点没有 address/port，绝不能把缺席字段插进模板串。
  ///
  /// 真机实证（2026-07-31）：卡片显示字面的 `undefined:undefined`。成因是同日那个 blocker 的另一面
  /// —— `ServerConfig` 放开了对 endpoint 节点 address/port 的协议盲必填校验之后，这句无保护的
  /// `` `${address}:${port}` `` 就把两个 undefined 插了出来。
  it('address/port 缺席时不得渲染出 undefined', () => {
    const out = exitAddrText({ address: undefined, port: undefined } as never);
    expect(out).toBeNull();
    expect(String(out)).not.toContain('undefined');
  });

  /// 🔴 TS 节点显示**设定的出口设备**，且判据是**配置**不是实时状态帧（2026-08-03 订正）。
  ///
  /// 曾用 STATUS 帧的 tailnet 自身 IP 作兜底，上机证伪：tsnet 卡在 `NoState` 时该帧恒空 ⇒ 卡片恒显 `—`，
  /// 而那正是用户最想知道「这条 TS 出网走哪」的时刻。出口设备是静态配置，与核跑不跑无关。
  ///
  /// 变异锁：改回读状态帧 / 让本函数再接一个 status 形参 → 本条与下一条一起失去意义（签名都对不上）。
  it('TS 节点取设定的出口设备，与核是否在跑无关', () => {
    expect(
      exitAddrText({
        address: '',
        port: 0,
        tailscaleSettings: { exitNode: 'iStoreOS-Sway' },
      } as never)
    ).toBe('iStoreOS-Sway');
    // 出口设备也可以填 IP（sing-box `exit_node` 接受 name 或 IP），原样透传不做解析。
    expect(
      exitAddrText({
        address: '',
        port: 0,
        tailscaleSettings: { exitNode: '100.123.174.107' },
      } as never)
    ).toBe('100.123.174.107');
  });

  /// 未设置出口设备 → `—`（调用方渲染占位符）。这是陈先生定的口径：没设就别猜。
  it('TS 节点未设出口设备 → null', () => {
    expect(exitAddrText({ address: '', port: 0, tailscaleSettings: {} } as never)).toBeNull();
    expect(
      exitAddrText({ address: '', port: 0, tailscaleSettings: { exitNode: '   ' } } as never)
    ).toBeNull();
    expect(exitAddrText({ address: '', port: 0 } as never)).toBeNull();
  });

  it('常规节点行为逐字不变', () => {
    expect(exitAddrText({ address: 'hk01.2polaris.com', port: 443 } as never)).toBe(
      'hk01.2polaris.com:443'
    );
  });

  /// port 缺席但 address 在 → 只显示 address。拼了冒号就是 `1.2.3.4:undefined`，同一类缺陷的另一半。
  it('有 address 无 port 时不拼冒号', () => {
    expect(exitAddrText({ address: '1.2.3.4', port: undefined } as never)).toBe('1.2.3.4');
  });

  it('server 缺席 → null', () => {
    expect(exitAddrText(undefined)).toBeNull();
    expect(exitAddrText(null)).toBeNull();
  });
});
