/**
 * SettingsDns 的纯判定单测 —— 三条都是**生产接线点本身**（组件直接调用这些导出函数，不是并行复刻），
 * 故删掉生产代码里的对应判定会让本文件转红，不会出现「测了个自造副本」的假绿。
 *
 *  1. `fakeIpTogglePatch`      —— 契约 L95「手改写 fakeIpTunAutoEnable:false」
 *  2. `needsFakeIpOffConfirm`  —— 契约 L95「TUN ON→OFF 一次性风险确认」
 *  3. `parseDnsServerSpec`     —— 契约 L94「非法标红不落盘」的判定源，须与后端 dns_spec.rs 同口径
 *  4. `normalizeDnsTimeoutInput` —— 与 store/sanitize.rs 的 1..60000 + round 同口径
 */
import { describe, it, expect } from 'vitest';
import {
  fakeIpTogglePatch,
  needsFakeIpOffConfirm,
  normalizeDnsTimeoutInput,
  parseDnsServerSpec,
} from './SettingsDns';

describe('fakeIpTogglePatch', () => {
  it('打开 FakeIP 时同写 fakeIpTunAutoEnable:false（消费一次性自动纠正资格）', () => {
    expect(fakeIpTogglePatch(true)).toEqual({
      enableFakeIp: true,
      fakeIpTunAutoEnable: false,
    });
  });

  it('关闭 FakeIP 时同写 fakeIpTunAutoEnable:false（这正是契约点名的场景：迁移用户手动关闭）', () => {
    expect(fakeIpTogglePatch(false)).toEqual({
      enableFakeIp: false,
      fakeIpTunAutoEnable: false,
    });
  });
});

describe('needsFakeIpOffConfirm', () => {
  it('TUN 下关闭 → 需要确认（节点将收真实 IP，机场可能拒连且客户端无法缓解）', () => {
    expect(needsFakeIpOffConfirm(false, 'tun')).toBe(true);
  });

  it('TUN 下开启 → 不确认（开启无风险）', () => {
    expect(needsFakeIpOffConfirm(true, 'tun')).toBe(false);
  });

  it.each(['systemProxy', 'manual', undefined])('非 TUN(%s) 关闭 → 不确认', (mode) => {
    expect(needsFakeIpOffConfirm(false, mode)).toBe(false);
  });
});

describe('parseDnsServerSpec（与后端 crates/config-engine/.../dns_spec.rs 同口径）', () => {
  it.each([
    ['https://1.1.1.1/dns-query', false],
    ['https://cloudflare-dns.com/dns-query', true],
    ['https://[2606:4700:4700::1111]/dns-query', false],
    ['tls://223.5.5.5:853', false],
    ['tls://dot.pub', true],
    ['udp://8.8.8.8', false],
    ['223.5.5.5', false],
    ['[2001:db8::1]', false],
    ['2001:db8::1', false],
  ])('%s 合法，isDomain=%s', (spec, isDomain) => {
    expect(parseDnsServerSpec(spec)).toEqual(expect.objectContaining({ isDomain }));
  });

  it.each([
    '',
    '   ',
    'doh.pub', // 裸域名：后端 parse_dns_server_spec 同样返回 None
    '8.8.8.8:53', // 无 scheme 的 IP:port —— 后端不接受，故不能在 UI 放行
    'https://1.1.1.1:70000/dns-query', // 端口越界
    'https://1.1.1.1:abc/dns-query', // 端口非数字
    'https:/1.1.1.1/dns-query', // 缺一个斜杠
    'https://', // 空 host
    'ftp://1.1.1.1',
  ])('%s 非法 → null（标红且不落盘）', (spec) => {
    expect(parseDnsServerSpec(spec)).toBeNull();
  });

  it('undefined / null 视为非法', () => {
    expect(parseDnsServerSpec(undefined)).toBeNull();
    expect(parseDnsServerSpec(null)).toBeNull();
  });

  it('IPv4 逐段 ≤255（256 判域名而非 IP，与后端 is_ipv4_segment 一致）', () => {
    expect(parseDnsServerSpec('https://256.1.1.1/dns-query')).toEqual(
      expect.objectContaining({ isDomain: true }),
    );
    expect(parseDnsServerSpec('256.1.1.1')).toBeNull(); // 裸形态既非 IP 也非可解析 spec
  });
});

describe('normalizeDnsTimeoutInput（与 crates/store/src/sanitize.rs:498-517 同口径）', () => {
  it('空 / 纯空格 → 删字段（用内核默认）', () => {
    expect(normalizeDnsTimeoutInput('')).toEqual({ value: undefined });
    expect(normalizeDnsTimeoutInput('   ')).toEqual({ value: undefined });
  });

  it.each([
    ['1', 1],
    ['5000', 5000],
    ['60000', 60000],
    ['1500.6', 1501], // 非整数四舍五入，对齐 sanitize 的 n.round()
  ])('%s → %s', (raw, value) => {
    expect(normalizeDnsTimeoutInput(raw)).toEqual({ value });
  });

  it.each(['0', '-1', '60001', 'abc', 'NaN', 'Infinity'])(
    '%s 越界/非数值 → null（标红且不落盘，避免后端静默删字段）',
    (raw) => {
      expect(normalizeDnsTimeoutInput(raw)).toBeNull();
    },
  );
});
