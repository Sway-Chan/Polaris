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
  isMeshTunnelNodeProtocol,
  meshTunnelNodeProtocols,
  PROTO_GROUP_ORDER,
  PROTO_OPTIONS,
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
  it('普通代理与组网隧道共同完全覆盖 PROTO_OPTIONS，且入口互斥', () => {
    const ordinary = PROTO_GROUP_ORDER.flatMap((g) => protosInGroup(g));
    const mesh = meshTunnelNodeProtocols();
    const laid = [...ordinary, ...mesh];
    expect([...laid].sort()).toEqual(PROTO_OPTIONS.map(([v]) => v).sort());
    expect(new Set(laid).size).toBe(laid.length);
    expect(mesh.every((proto) => !ordinary.includes(proto))).toBe(true);
  });

  it('Custom 单独一组且置底', () => {
    expect(PROTO_GROUP_ORDER[PROTO_GROUP_ORDER.length - 1]).toBe('custom');
    expect(protosInGroup('custom')).toEqual(['custom']);
  });

  it('组网隧道节点选项只含 OpenConnect / OpenVPN，并按展示名排序', () => {
    expect(meshTunnelNodeProtocols()).toEqual(['openconnect', 'openvpn-client']);
  });

  it('所有普通分组都按展示名排序，NaiveProxy 归入常用', () => {
    const label = new Map(PROTO_OPTIONS);
    for (const group of PROTO_GROUP_ORDER) {
      const names = protosInGroup(group).map((p) => label.get(p)!);
      expect(names).toEqual([...names].sort((a, b) => a.localeCompare(b, 'en', { sensitivity: 'base' })));
    }
    expect(protosInGroup('common')).toContain('naive');
  });

  it('OpenConnect 协议名保持简洁，厂商方言只在表单内选择', () => {
    expect(new Map(PROTO_OPTIONS).get('openconnect')).toBe('OpenConnect');
  });

  it('入口判据只表达组网表单归属，不存在 vpn 协议分组', () => {
    expect(PROTO_GROUP_ORDER).toEqual(['common', 'proxy', 'custom']);
    expect(isMeshTunnelNodeProtocol('openconnect')).toBe(true);
    expect(isMeshTunnelNodeProtocol('openvpn-client')).toBe(true);
    expect(isMeshTunnelNodeProtocol('vless')).toBe(false);
  });
});
