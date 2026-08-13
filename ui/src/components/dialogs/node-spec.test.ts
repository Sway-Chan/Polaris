/**
 * `describeProbeResult`（C10 custom 协议内核兼容性 probe 的展示态映射）纯函数单测。
 *
 * 本仓 vitest 是 `environment:'node'`（无 jsdom），`NodeDialog` 引入即会因模块加载期碰 DOM 而炸
 * （同 `field-switch-disabled.test.tsx` 文档所述 WarpDialog/WgDialog 的先例），故渲染出来的按钮/
 * 提示条测不到；能测、也必须测的是它背后这个纯映射——它决定「用户看到支持/不支持/无法判定的
 * 哪一句」，映射错了整块 UI 就会文不对题。
 */
import { describe, expect, it } from 'vitest';
import {
  describeProbeResult,
  PROTO_GROUP_ORDER,
  PROTO_OPTIONS,
  protoGroupsForNodeForm,
  protosInGroup,
  type ProbeOutboundResult,
} from './node-spec';

describe('describeProbeResult', () => {
  it('ok:true → supported，且不携带任何诊断字段', () => {
    const r: ProbeOutboundResult = { ok: true };
    expect(describeProbeResult(r)).toEqual({ kind: 'supported' });
  });

  it('indeterminate:true → indeterminate，即便后端捎带了 error 文案也不采信', () => {
    // 后端目前对这一态固定回一句中文（`probe_verdict` 的 Indeterminate 分支）；调用方必须只认
    // `indeterminate` 标志位、自己用本地 i18n 渲染文案，不能把这句话透出去——否则非中文界面会看到
    // 一句写死的中文。这条测试钉住「不采信」这个决策本身。
    const r: ProbeOutboundResult = {
      ok: false,
      indeterminate: true,
      error: '内核不可用或超时，无法判定兼容性',
    };
    const d = describeProbeResult(r);
    expect(d.kind).toBe('indeterminate');
    expect(d).not.toHaveProperty('message');
  });

  it('ok:false 带 errorPath → unsupported，path/message/raw 逐字段透传', () => {
    const r: ProbeOutboundResult = {
      ok: false,
      error: 'json: unknown field "bogus_field"',
      errorPath: 'outbounds[0].bogus_field',
      errorRaw:
        'FATAL[0000] decode config at /tmp/x.json: outbounds[0].bogus_field: json: unknown field "bogus_field"',
    };
    expect(describeProbeResult(r)).toEqual({
      kind: 'unsupported',
      keyPath: 'outbounds[0].bogus_field',
      message: 'json: unknown field "bogus_field"',
      raw: 'FATAL[0000] decode config at /tmp/x.json: outbounds[0].bogus_field: json: unknown field "bogus_field"',
    });
  });

  it('ok:false 无 errorPath（解析不出键路径）→ unsupported.keyPath 是 undefined，不是空串', () => {
    const r: ProbeOutboundResult = {
      ok: false,
      error: 'invalid character \'t\' looking for beginning of object key string: row 1, column 3',
      errorRaw:
        'FATAL[0000] decode config at /tmp/x.json: invalid character \'t\' looking for beginning of object key string: row 1, column 3',
    };
    const d = describeProbeResult(r);
    expect(d.kind).toBe('unsupported');
    if (d.kind === 'unsupported') {
      expect(d.keyPath).toBeUndefined();
      expect(d.message).toContain('invalid character');
    }
  });

  it('ok:false 且 errorRaw 缺失（理论兜底腿）→ raw 回落到 error', () => {
    // 正常路径 errorRaw 恒随 Unsupported 下发（Rust 侧 `probe_verdict`），这里只验证映射函数自身
    // 在契约被违反时不炸、不产出 undefined——`raw` 是兜底展示位，宁可重复 error 也不能空着。
    const r: ProbeOutboundResult = { ok: false, error: 'boom' };
    expect(describeProbeResult(r)).toEqual({
      kind: 'unsupported',
      keyPath: undefined,
      message: 'boom',
      raw: 'boom',
    });
  });
});

describe('协议下拉的分组与顺序', () => {
  it('分组是 PROTO_OPTIONS 的**完全划分** —— 不重不漏', () => {
    // 漏一个 = 用户在对话框里根本建不出那个协议的节点（下拉里没有它），而其余测试全绿：
    // 它们遍历的是扁平的 PROTO_OPTIONS，看不见渲染端实际摆出来的是哪些。
    const laid = PROTO_GROUP_ORDER.flatMap((g) => protosInGroup(g));
    expect([...laid].sort()).toEqual(PROTO_OPTIONS.map(([v]) => v).sort());
    expect(new Set(laid).size).toBe(laid.length);
  });

  it('Custom 单独一组且置底', () => {
    expect(PROTO_GROUP_ORDER[PROTO_GROUP_ORDER.length - 1]).toBe('custom');
    expect(protosInGroup('custom')).toEqual(['custom']);
  });

  it('VPN 组 = 落 sing-box endpoints[] 的那两个（判据可推，不是口味）', () => {
    expect(protosInGroup('vpn')).toEqual(['openconnect', 'openvpn-client']);
  });

  it('「其他代理」组按展示名排序，常用组保持既定顺序（不排字母序）', () => {
    const label = new Map(PROTO_OPTIONS);
    const names = protosInGroup('proxy').map((p) => label.get(p)!);
    expect(names).toEqual([...names].sort((a, b) => a.localeCompare(b, 'en', { sensitivity: 'base' })));
    // 常用组的价值就是「最常用的排最前」，字母序会把 VLESS 推到末尾。
    expect(protosInGroup('common')[0]).toBe('vless');
  });

  it('OpenConnect 的标签带上 AnyConnect —— 否则找它的人扫不到', () => {
    // 内核不设独立的 anyconnect 类型，它是 OpenConnect 的一个 flavor（六选一，默认就是它）。
    expect(new Map(PROTO_OPTIONS).get('openconnect')).toContain('AnyConnect');
  });

  it('创建入口互斥：普通节点不含 VPN，组网接入只含 VPN；编辑态保留全集', () => {
    expect(protoGroupsForNodeForm(false)).toEqual(['common', 'proxy', 'custom']);
    expect(protoGroupsForNodeForm(false, 'openconnect')).toEqual(['vpn']);
    expect(protoGroupsForNodeForm(false, 'openvpn-client')).toEqual(['vpn']);
    expect(protoGroupsForNodeForm(true)).toEqual(PROTO_GROUP_ORDER);
  });
});
