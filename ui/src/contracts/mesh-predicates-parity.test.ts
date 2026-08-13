/**
 * 两条路由分类谓词的**跨语言对拍**：TS 的 `MESH_PROTOCOLS` / `ENDPOINT_LEG_PROTOCOLS` 必须与
 * Rust 的 `is_mesh_protocol` / `lands_in_endpoints` 成员集逐条一致。
 *
 * # 为什么必须有这道门
 *
 * 这两个概念此前是**一个**谓词（`isEndpointProtocol` / `is_endpoint_protocol`），名字说的是
 * 「落 sing-box `endpoints[]`」、成员集给的却是「组网」。两者在 openconnect / openvpn-client 上
 * 不重合，消费点按名字挑谓词，同一根因下三处各错各的（临时测速核塞错数组致整核 FATAL、
 * detour 悬空引用、承流播种漏项致该重启时不重启）。
 *
 * 拆成两条之后，新的失效模式是**两语言各自漂移**：前端把某协议挪进组网、后端没挪，UI 把它排进
 * 组网页签并按组网语义渲染，而生成侧不给它发 force-route —— 页面说它的网段可达，实际不可达。
 * 前后端各持一份清单的先例在本仓有过（issue #191 的 scheme 白名单），故同样以「常量 + 源码对拍」
 * 保证同源。
 *
 * # 判据从 Rust **源码**解析，不写第二份清单
 *
 * 写第二份就是把「两处清单要一致」换成「三处清单要一致」。wire 名同样从源码取（枚举级
 * `rename_all = "lowercase"` 折叠，`OpenvpnClient` 另有 per-variant `rename`），不在这里复刻规则。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { MESH_PROTOCOLS, ENDPOINT_LEG_PROTOCOLS } from '@/domain/endpoint-routes';

const SRC = fileURLToPath(
  new URL('../../../crates/config-engine/src/user_config/server_config.rs', import.meta.url)
);

const rust = readFileSync(SRC, 'utf8');

/** 变体名 → sing-box/落盘 wire 名。per-variant `rename` 优先，否则枚举级 lowercase。 */
function wireNames(): Map<string, string> {
  const enumBody = /pub enum Protocol \{([\s\S]*?)\n\}/.exec(rust)?.[1] ?? '';
  const map = new Map<string, string>();
  let pendingRename: string | null = null;
  for (const line of enumBody.split('\n')) {
    const t = line.trim();
    if (t.startsWith('//') || t.startsWith('///')) continue;
    const rename = /#\[serde\(rename = "([^"]+)"/.exec(t);
    if (rename) {
      pendingRename = rename[1];
      continue;
    }
    const variant = /^([A-Z][A-Za-z0-9]*)\s*,/.exec(t);
    if (variant) {
      map.set(variant[1], pendingRename ?? variant[1].toLowerCase());
      pendingRename = null;
    }
  }
  return map;
}

/** 某个 `pub fn <name>` 的 `matches!` 里列出的协议变体 → wire 名集合。 */
function membersOf(fnName: string): string[] {
  const names = wireNames();
  // 函数体取到第一个 `}` 收尾的那一行为止；两个谓词都是单个 `matches!` 表达式。
  const body = new RegExp(`pub fn ${fnName}\\([^)]*\\) -> bool \\{([\\s\\S]*?)\\n\\}`).exec(rust)?.[1];
  if (body === undefined) throw new Error(`Rust 源码里找不到 ${fnName} —— 改名了？对拍失效`);
  return [...body.matchAll(/Protocol::([A-Z][A-Za-z0-9]*)/g)]
    .map((m) => {
      const w = names.get(m[1]);
      if (!w) throw new Error(`枚举里没有变体 ${m[1]} —— 解析器与源码脱节`);
      return w;
    })
    .sort();
}

describe('组网 / endpoint 腿谓词跨语言一致', () => {
  it('MESH_PROTOCOLS ⟺ Rust is_mesh_protocol', () => {
    expect([...MESH_PROTOCOLS].sort()).toEqual(membersOf('is_mesh_protocol'));
  });

  it('ENDPOINT_LEG_PROTOCOLS ⟺ Rust lands_in_endpoints', () => {
    expect([...ENDPOINT_LEG_PROTOCOLS].sort()).toEqual(membersOf('lands_in_endpoints'));
  });

  it('组网 ⊂ endpoint 腿（真子集，两侧都该成立）', () => {
    for (const p of MESH_PROTOCOLS) expect(ENDPOINT_LEG_PROTOCOLS).toContain(p);
    expect(ENDPOINT_LEG_PROTOCOLS.length).toBeGreaterThan(MESH_PROTOCOLS.length);
  });

  it('解析器自检：wire 名映射真的解析到了 per-variant rename', () => {
    // 这条钉住解析器本身。`OpenvpnClient` 是全枚举唯一需要 per-variant rename 的变体
    // （枚举级 lowercase 会把它折成 `openvpnclient`）—— 解析器若漏读 rename，这里先红，
    // 而不是让上面两条以「两边都错成 openvpnclient」的方式空过。
    expect(wireNames().get('OpenvpnClient')).toBe('openvpn-client');
    expect(wireNames().get('Wireguard')).toBe('wireguard');
  });
});
