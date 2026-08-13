/**
 * 组网重叠角标谓词的判定断言。契约 §Rules「角标 — 组网 force-route 重叠(meshOverlapRuleIds)」。
 *
 * 重点不是「函数跑通」，而是钉住三条会静默失效的判据：**前缀相交（非字面量相等）**、
 * **跨族不相交**、**禁用规则不算**。
 */
import { describe, it, expect } from 'vitest';
import type { Rule } from '@/contracts/types';
import { cidrsOverlap, cidrOverlapsAny, meshOverlapRuleIds } from './mesh-rule-overlap';

function rule(id: string, values: string[], enabled = true, type: Rule['type'] = 'ipCidr'): Rule {
  return { id, type, values, action: 'proxy', enabled } as Rule;
}

describe('cidrsOverlap', () => {
  it('包含关系相交（这正是字面量比对答不出的那一类）', () => {
    // 变异守卫：把实现退化成 `a.trim() === b.trim()` → 本例转红。
    expect(cidrsOverlap('10.0.0.0/8', '10.8.0.0/24')).toBe(true);
    expect(cidrsOverlap('10.8.0.0/24', '10.0.0.0/8')).toBe(true);
  });

  it('不相交的同族段 → false', () => {
    expect(cidrsOverlap('10.8.0.0/24', '10.9.0.0/24')).toBe(false);
    expect(cidrsOverlap('192.168.1.0/24', '10.0.0.0/8')).toBe(false);
  });

  it('裸 IP 视作 /32 与 /128', () => {
    expect(cidrsOverlap('10.8.0.5', '10.8.0.0/24')).toBe(true);
    expect(cidrsOverlap('10.9.0.5', '10.8.0.0/24')).toBe(false);
    expect(cidrsOverlap('fd7a:115c:a1e0::1', 'fd7a:115c:a1e0::/48')).toBe(true);
  });

  it('全网段覆盖一切（force-route 全隧道节点）', () => {
    expect(cidrsOverlap('0.0.0.0/0', '203.0.113.7/32')).toBe(true);
    expect(cidrsOverlap('::/0', 'fd7a::1/128')).toBe(true);
  });

  it('v6 前缀相交 / 不相交', () => {
    expect(cidrsOverlap('fd7a:115c:a1e0::/48', 'fd7a:115c:a1e0:ab12::/64')).toBe(true);
    // fd00::/8 与 fd7a:… 的前 8 bit 同为 0xfd → **相交**（Tailscale 的 fd7a 段本就落在 ULA fd00::/8 内）。
    expect(cidrsOverlap('fd7a:115c:a1e0::/48', 'fd00::/8')).toBe(true);
    // 真不相交要看第 8 bit：fc(…1100) vs fd(…1101)。
    expect(cidrsOverlap('fd7a:115c:a1e0::/48', 'fc00::/8')).toBe(false);
    expect(cidrsOverlap('fd7a:115c:a1e0::/48', '2001:db8::/32')).toBe(false);
  });

  it('v6 前缀落在 16-bit 组中间时按位比对（非整组）', () => {
    // /36 = 前两组整取 + 第三组只取高 4 bit。守住 `groupMask` 的部分掩码分支：
    // 若把它写成「整组取或整组丢」，1000 与 1fff 会判不相交、1000 与 2000 会判相交 → 两条同时转红。
    expect(cidrsOverlap('2001:db8:1000::/36', '2001:db8:1fff::/36')).toBe(true);
    expect(cidrsOverlap('2001:db8:1000::/36', '2001:db8:2000::/36')).toBe(false);
  });

  it('跨族恒不相交（v4 与 v6 不得互判命中）', () => {
    // 变异守卫：若把两族比对写成「任一 parse 成功即比」会误判 → 转红。
    expect(cidrsOverlap('10.0.0.0/8', 'fd7a::/16')).toBe(false);
    expect(cidrsOverlap('::/0', '0.0.0.0/0')).toBe(false);
  });

  it('非法输入恒 false（不抛、不误命中）', () => {
    expect(cidrsOverlap('999.1.1.1/8', '10.0.0.0/8')).toBe(false);
    expect(cidrsOverlap('', '10.0.0.0/8')).toBe(false);
    expect(cidrsOverlap('10.0.0.0/33', '10.0.0.0/8')).toBe(false);
    expect(cidrsOverlap('not-an-ip', '10.0.0.0/8')).toBe(false);
  });
});

describe('cidrOverlapsAny', () => {
  it('候选集任一相交即真；空候选集恒假', () => {
    expect(cidrOverlapsAny('10.8.0.1', ['192.168.0.0/16', '10.8.0.0/24'])).toBe(true);
    expect(cidrOverlapsAny('172.16.0.1', ['192.168.0.0/16', '10.8.0.0/24'])).toBe(false);
    expect(cidrOverlapsAny('10.8.0.1', [])).toBe(false);
  });
});

describe('meshOverlapRuleIds', () => {
  const mesh = ['10.8.0.0/24', 'fd7a:115c:a1e0::/48'];

  it('已启用 + ipCidr 与组网段相交 → 标记', () => {
    const ids = meshOverlapRuleIds([rule('a', ['10.8.0.0/32'])], mesh);
    expect(ids).toEqual(new Set(['a']));
  });

  it('禁用规则不标（不下发就抢不走路由）', () => {
    // 变异守卫：删掉 `if (!r.enabled) continue` → 转红。
    expect(meshOverlapRuleIds([rule('a', ['10.8.0.0/32'], false)], mesh)).toEqual(new Set());
  });

  it('非 ipCidr 条件不标（域名/端口不在 IP 路由判定面上）', () => {
    expect(meshOverlapRuleIds([rule('a', ['10.8.0.0/24'], true, 'domain')], mesh)).toEqual(
      new Set()
    );
  });

  it('多条件规则里只要有一条 ipCidr 相交就标', () => {
    const multi: Rule = {
      id: 'm',
      type: 'domain',
      values: ['example.com'],
      conditions: [
        { type: 'domain', values: ['example.com'] },
        { type: 'ipCidr', values: ['203.0.113.0/24', '10.8.0.9'] },
      ],
      action: 'proxy',
      enabled: true,
    } as Rule;
    expect(meshOverlapRuleIds([multi], mesh)).toEqual(new Set(['m']));
  });

  it('无组网段（没开组网/无 force-route）→ 恒空，不逐规则空转', () => {
    expect(meshOverlapRuleIds([rule('a', ['10.8.0.0/24'])], [])).toEqual(new Set());
  });

  it('不相交的规则不标（避免全列表刷警告的噪音角标）', () => {
    expect(meshOverlapRuleIds([rule('a', ['192.168.1.0/24'])], mesh)).toEqual(new Set());
  });
});
