import { describe, expect, it } from 'vitest';
import {
  resolveStatusBarExitIp,
  resolveStatusBarLatencyText,
  resolveStatusBarNodeName,
} from './status-bar-display';

describe('resolveStatusBarNodeName', () => {
  it('直连选中 → 显「直连」标签，不看是否有 currentServer', () => {
    expect(resolveStatusBarNodeName(true, undefined, '直连', '请先配置服务器')).toBe('直连');
    expect(resolveStatusBarNodeName(true, '香港 01', '直连', '请先配置服务器')).toBe('直连');
  });

  it('非直连 + 有节点 → 节点名（不随连接态切换成"未连接"）', () => {
    expect(resolveStatusBarNodeName(false, '香港 01', '直连', '请先配置服务器')).toBe('香港 01');
  });

  it('非直连 + 无节点 → 占位符，而非重复的"未连接"状态文案', () => {
    expect(resolveStatusBarNodeName(false, undefined, '直连', '请先配置服务器')).toBe(
      '请先配置服务器'
    );
  });

  /**
   * 阻断选中 → 显「阻断」，**不得**落到占位符。
   *
   * 这是「不能谎报」的核心一条：哨兵不是节点 id ⇒ serverName 恒 undefined ⇒ 漏判就会显示
   * 「请先配置服务器」，而用户明明选了一个有效出口且核在跑。
   * 变异锁：删掉 `if (blockSelected) return blockLabel` → 转红。
   */
  it('阻断选中 → 显「阻断」，不落占位符', () => {
    expect(
      resolveStatusBarNodeName(false, undefined, '直连', '请先配置服务器', true, '阻断')
    ).toBe('阻断');
  });

  /**
   * 阻断**不得**被显示成「直连」—— 两者都是哨兵出口，合并判据会让状态栏把「流量全丢」
   * 说成「本机直连」，是最坏的一种谎报。
   */
  it('阻断与直连不得互相冒充', () => {
    expect(resolveStatusBarNodeName(false, undefined, '直连', '占位', true, '阻断')).not.toBe(
      '直连'
    );
    // direct 优先级在前：两个标志同真时（不该发生）显直连，但绝不能显占位符。
    expect(resolveStatusBarNodeName(true, undefined, '直连', '占位', true, '阻断')).toBe('直连');
  });

  /** 两个哨兵都不选时，新增的两个尾参默认值不得改变原有三条口径（回归保护）。 */
  it('尾参缺省时行为与改造前一致', () => {
    expect(resolveStatusBarNodeName(false, '香港 01', '直连', '占位')).toBe('香港 01');
    expect(resolveStatusBarNodeName(false, undefined, '直连', '占位')).toBe('占位');
  });
});

describe('resolveStatusBarLatencyText', () => {
  it('断开时恒 "—"，即便延迟值残留（不清空旧值也不能显示）', () => {
    expect(resolveStatusBarLatencyText(false, 86)).toBe('—');
    expect(resolveStatusBarLatencyText(false, null)).toBe('—');
    expect(resolveStatusBarLatencyText(false, undefined)).toBe('—');
  });

  it('连接 + 有效延迟 → "N ms"', () => {
    expect(resolveStatusBarLatencyText(true, 86)).toBe('86 ms');
  });

  it('连接 + 未测/超时 → "—"', () => {
    expect(resolveStatusBarLatencyText(true, null)).toBe('—');
    expect(resolveStatusBarLatencyText(true, undefined)).toBe('—');
  });
});

describe('resolveStatusBarExitIp', () => {
  it('连接态只认代理 IP，不回落本地 IP（探测中不得把本地 IP 当出口展示）', () => {
    expect(resolveStatusBarExitIp(true, '203.0.113.1', '198.51.100.1')).toBe('203.0.113.1');
    expect(resolveStatusBarExitIp(true, undefined, '198.51.100.1')).toBe('—');
  });

  it('未连接态只认本地 IP，不回落代理 IP', () => {
    expect(resolveStatusBarExitIp(false, '203.0.113.1', '198.51.100.1')).toBe('198.51.100.1');
    expect(resolveStatusBarExitIp(false, '203.0.113.1', undefined)).toBe('—');
  });
});
