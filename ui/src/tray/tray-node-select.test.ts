import { describe, expect, it } from 'vitest';
import {
  deriveTrayConnectButton,
  isTrayServerConfigured,
  normalizeLatency,
  protoShort,
  resolveTrayExitIp,
} from './tray-node-select';
import { BLOCK_SERVER_ID, DIRECT_SERVER_ID } from '@/domain/direct-selection';

describe('isTrayServerConfigured', () => {
  it('direct 哨兵选中时恒已配置，即便零真实节点', () => {
    expect(isTrayServerConfigured(DIRECT_SERVER_ID, 0)).toBe(true);
  });

  /**
   * block 哨兵同理：阻断出口不需要节点承载（config-engine 已豁免其存在性校验）。
   *
   * 漏了这条 ⇒ 零节点 + 选阻断时托盘连接按钮变灰，用户既连不上也看不出为什么。
   * 变异锁：把 isTrayServerConfigured 里的 isSentinelSelection 换回 isDirectSelection → 转红。
   */
  it('block 哨兵选中时恒已配置，即便零真实节点', () => {
    expect(isTrayServerConfigured(BLOCK_SERVER_ID, 0)).toBe(true);
  });

  it('非 direct 且有节点 → 已配置', () => {
    expect(isTrayServerConfigured('srv-1', 3)).toBe(true);
  });

  it('非 direct 且零节点 → 未配置', () => {
    expect(isTrayServerConfigured('srv-1', 0)).toBe(false);
    expect(isTrayServerConfigured(null, 0)).toBe(false);
    expect(isTrayServerConfigured(undefined, 0)).toBe(false);
  });
});

describe('normalizeLatency', () => {
  it('负值（超时哨兵 -1）归一成 null', () => {
    expect(normalizeLatency(-1)).toBeNull();
  });

  it('非负数值原样透传', () => {
    expect(normalizeLatency(0)).toBe(0);
    expect(normalizeLatency(86)).toBe(86);
  });

  it('null/undefined 原样透传（未测 / 显式无结果）', () => {
    expect(normalizeLatency(null)).toBeNull();
    expect(normalizeLatency(undefined)).toBeUndefined();
  });
});

describe('protoShort', () => {
  it('上游 短写表命中：WG / TS / SS / Hy2', () => {
    expect(protoShort('wireguard')).toBe('WG');
    expect(protoShort('tailscale')).toBe('TS');
    expect(protoShort('shadowsocks')).toBe('SS');
    expect(protoShort('hysteria2')).toBe('Hy2');
  });

  it('大小写不敏感（命中表用小写归一）', () => {
    expect(protoShort('WireGuard')).toBe('WG');
    expect(protoShort('HYSTERIA2')).toBe('Hy2');
  });

  it('未命中短写表 → 大写回退（对齐 上游 toUpperCase）', () => {
    expect(protoShort('vless')).toBe('VLESS');
    expect(protoShort('trojan')).toBe('TROJAN');
    expect(protoShort('tuic')).toBe('TUIC');
  });

  it('空 / null / undefined → 空串（不抛）', () => {
    expect(protoShort('')).toBe('');
    expect(protoShort(null)).toBe('');
    expect(protoShort(undefined)).toBe('');
  });
});

/**
 * 托盘连接钮 —— 本次真机事故里托盘那一半的回归门。
 *
 * 旧实现 `if (busy || ...) return; if (running) stop else start`：
 *  - 起核期 `running` 恒 false ⇒ 点击走 **start** 分支 ⇒ 在已有起核腿之上再叠一个核；
 *  - 本窗发起时 `busy` 又把按钮一律置灰 ⇒ 用户连取消都点不了。
 * 两条一起，构成「启动卡死阶段无法关闭启动过程」在托盘侧的形态。
 */
describe('deriveTrayConnectButton', () => {
  const base = {
    running: false,
    backendStarting: false,
    pending: null as 'start' | 'stop' | null,
    serverConfigured: true,
  };

  it('空闲 + 已配置 → 可点，action=start', () => {
    const s = deriveTrayConnectButton(base);
    expect(s.action).toBe('start');
    expect(s.disabled).toBe(false);
  });

  it('未配置出口 → start 但置灰', () => {
    expect(deriveTrayConnectButton({ ...base, serverConfigured: false }).disabled).toBe(true);
  });

  it('已连接 → action=stop', () => {
    expect(deriveTrayConnectButton({ ...base, running: true }).action).toBe('stop');
  });

  /**
   * **本窗发起的起核在飞** → 必须是可点的取消。
   * 变异：把 `pending==='start'` 从 starting 来源里删掉 → action 退回 'start'（叠第二次起核）→ 转红；
   * 或把按钮改回 `disabled={busy}` 语义（starting 置灰）→ disabled 断言转红。
   */
  it('本窗正在启动 → action=cancel 且可点（不是再叠一次 start）', () => {
    const s = deriveTrayConnectButton({ ...base, pending: 'start' });
    expect(s.action).toBe('cancel');
    expect(s.action).not.toBe('start');
    expect(s.disabled).toBe(false);
  });

  /**
   * **别的入口（主窗/自动连接/崩溃自愈）发起的起核在飞** → 托盘同样必须给取消，而不是 start。
   * 托盘无 store 可共享，这条只能靠后端 `ProxyStatus.starting` 得知。
   * 变异：把 `backendStarting` 从 starting 来源里删掉 → 转红（这正是「从主窗点连接、再开托盘点一下」
   * 就多起一个核的那条路径）。
   */
  it('后端报起核在飞（他窗发起）→ 托盘也必须是 cancel', () => {
    const s = deriveTrayConnectButton({ ...base, backendStarting: true });
    expect(s.action).toBe('cancel');
    expect(s.disabled).toBe(false);
  });

  it('起核在飞时即便未配置出口也能取消（核都在起了，配置不该妨碍叫停）', () => {
    const s = deriveTrayConnectButton({
      ...base,
      backendStarting: true,
      serverConfigured: false,
    });
    expect(s.action).toBe('cancel');
    expect(s.disabled).toBe(false);
  });

  /** 取消途中（stop 已发、start 还在飞）→ 不可重复点，避免每点一次多发一条 stop。 */
  it('取消途中 → stopping 压过 starting，置灰且无可操作', () => {
    const s = deriveTrayConnectButton({ ...base, backendStarting: true, pending: 'stop' });
    expect(s.kind).toBe('stopping');
    expect(s.disabled).toBe(true);
    expect(s.action).toBe('none');
  });

  it('停止在飞 → 置灰', () => {
    expect(deriveTrayConnectButton({ ...base, running: true, pending: 'stop' }).disabled).toBe(true);
  });
});

describe('resolveTrayExitIp', () => {
  it('已连接只认 proxy 腿，绝不回落 direct（回落 = 把本机 IP 冒充成出口）', () => {
    expect(resolveTrayExitIp(true, '1.1.1.1', '9.9.9.9')).toBe('1.1.1.1');
    // 代理出口尚未探到（收敛窗口 / 探测失败）→ 留空，**不许**吐本机 IP。
    expect(resolveTrayExitIp(true, undefined, '9.9.9.9')).toBe('');
  });

  it('未连接只认 direct 腿（此时不存在「代理出口」）', () => {
    expect(resolveTrayExitIp(false, '1.1.1.1', '9.9.9.9')).toBe('9.9.9.9');
    expect(resolveTrayExitIp(false, '1.1.1.1', undefined)).toBe('');
  });

  it('两腿皆无 → 空串（托盘卡以空串表示整段不渲染，不是 "—"）', () => {
    expect(resolveTrayExitIp(true, undefined, undefined)).toBe('');
    expect(resolveTrayExitIp(false, undefined, undefined)).toBe('');
  });
});
