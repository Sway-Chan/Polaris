/**
 * WARP 的 `system:true` 否决 —— **跨语言同源门**。
 *
 * # 这道门守的是什么
 *
 * WARP 是 anycast 出口、不是子网路由器：给它发 `system:true` 会与主 TUN 抢内核 utun ⇒
 * `post-start endpoint/wireguard[Cloudflare WARP]: Connect: resource busy` **FATAL，内核起不来**
 * （真机实证记在 `src/domain/warp.ts` 的 `isWarpServer` 文档里）。于是两侧各有一道否决：
 *  - 前端 `meshUsesSystemInterface`（`src/domain/endpoint-routes.ts`）；
 *  - Rust `mesh_uses_system_interface`（`crates/config-engine/src/builder/endpoint_routes.rs`）。
 *
 * **Rust 那道才是真正承重的**：config-engine 是 `system:true` 的唯一发射方，而落盘的 `servers[]`
 * 有三条不经渲染端的入口 —— 导入配置 / 手改 `config.json` / 从 上游 迁移的配置。本门落地前，
 * Rust 侧只读 `reverseMesh`、**完全没有 WARP 检测**（原注释写着「WARP 检测后续补」），
 * 前端那道否决对这三条腿一点用没有。这不是显示问题，是阻断级。
 *
 * # 为什么是「读两边源码」而不是再抄一份镜像常量
 *
 * 抄镜像只是把漂移面往后挪一格。本仓既有手法（`unlock-detection.test.ts` /
 * `protocol-settings-coverage.test.ts` / `user-config-fields.test.ts`）都是直接把源码当真值读进来解析。
 * 这里守的正是「同一份数据被两层各自校验、判据不来自同一个真值源」这类缺陷 ——
 * 门自己再造一份第三副本就自相矛盾了。
 *
 * # 五把锁
 *
 *  1. **解析器自检**：四个函数体解析不到就抛错。解析不到必须转红，不得「读不到就跳过」——
 *     那样任一侧改名门就静默消失，「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 *  2. **常量字面量**：两侧 `WARP_ENDPOINT_DOMAIN` / `WARP_MTU` 逐字相等；且
 *     `crates/mesh/src/warp.rs` **不得**重新定义它们（域名只 re-export config-engine 的那份，MTU
 *     完全归生成器所有）—— 防注册草稿、表单和生成器各塞一份默认值。
 *  3. **判据字段集**：两个谓词体读的 ServerConfig 字段集必须逐项相等（归一化后比较，
 *     `wireguard_settings` ≡ `wireguardSettings`）。单侧加/减判据字段即红。
 *  4. **域名兜底腿**：两个谓词体都必须引用 `WARP_ENDPOINT_DOMAIN`。这条单独钉是因为它守的是
 *     **旧 / 导入 / 迁移来的 WARP 节点没有 `warpDevice` 标记**这个事实 —— 少了它，恰好是会 FATAL 的那类节点漏判。
 *  5. **否决接线**：两侧 `meshUsesSystemInterface` / `mesh_uses_system_interface` 体内必须各自调用
 *     WARP 谓词。**这是本缺陷的直接对应锁**：Rust 侧把那一行删掉 → 立刻转红。
 *
 * # 注释必须先剔掉（承重步骤，不是整洁癖）
 *
 * 两侧的 `meshUsesSystemInterface` 注释里都写着 `isWarpServer` / `crate::warp`。不剔注释的话，
 * 锁 5 会被注释「盖绿」—— 把否决那行代码删干净、注释留着，门照样报绿。
 * （同款教训见 `protocol-settings-coverage.test.ts` 的 `stripComments` 文档。）
 *
 * # 这道门抓不到什么（如实记）
 *
 *  - 判据是**字段集 + 引用**，不是语义等价。把 Rust 的 `!=` 写成 `==`（协议闸反向）字段集不变 → 门绿。
 *    语义由 Rust 单测钉（`builder/endpoint_routes.rs` 的 `wg_reverse_mesh_system_vetoed_only_for_warp`
 *    含反向对照：普通 WG 的 `reverseMesh:true` 仍须返 true）。
 *  - 只覆盖 `isWarpServer` / `meshUsesSystemInterface` 这一对腿。`findWarpNode` / `warpSlotTaken`
 *    是渲染端独有的单例闸，Rust 无对应物，不在射程内。
 *  - 谓词体里出现字段名但不参与判断（`const x = server.address;` 写了不用）算作「用到」。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { WARP_ENDPOINT_DOMAIN, WARP_MTU } from '../domain/warp';

function read(rel: string): string {
  return readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
}

const TS_WARP = read('../domain/warp.ts');
const TS_ENDPOINT_ROUTES = read('../domain/endpoint-routes.ts');
const RUST_WARP = read('../../../crates/config-engine/src/warp.rs');
const RUST_ENDPOINT_ROUTES = read('../../../crates/config-engine/src/builder/endpoint_routes.rs');
const RUST_MESH_WARP = read('../../../crates/mesh/src/warp.rs');

/** 块注释 + 行注释剔除（Rust 的 `//!` / `///` 同属行注释）。锁 5 全靠这一步，见文件头。 */
function stripComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');
}

/**
 * 取函数体：锚点 → 参数表配对 `)` → 其后第一个 `{` 起花括号配对。
 *
 * 先过参数表是**必须**的：`isWarpServer` 的形参是内联对象类型（`server: { protocol?: string; … }`），
 * 直接找「锚点后第一个 `{`」会取到形参类型体而不是函数体。
 */
function fnBody(src: string, anchor: string, label: string): string {
  const stripped = stripComments(src);
  const at = stripped.indexOf(anchor);
  expect(at, `${label}：解析不到 \`${anchor}\`（改名/重构了？）—— 必须转红，不得静默放行`).toBeGreaterThanOrEqual(0);

  let i = stripped.indexOf('(', at);
  let depth = 0;
  for (; i < stripped.length; i++) {
    if (stripped[i] === '(') depth++;
    else if (stripped[i] === ')' && --depth === 0) break;
  }
  const open = stripped.indexOf('{', i);
  expect(open, `${label}：参数表之后找不到函数体 \`{\``).toBeGreaterThan(0);

  depth = 0;
  for (let j = open; j < stripped.length; j++) {
    if (stripped[j] === '{') depth++;
    else if (stripped[j] === '}' && --depth === 0) return stripped.slice(open + 1, j);
  }
  throw new Error(`${label}：函数体花括号不配对 —— 解析器失效，必须转红`);
}

/**
 * 函数体里读到的**字段**（≠ 方法调用）：取 `.ident` / `?.ident`，丢掉后面紧跟 `(` 的那些
 * （`toLowerCase()` / `as_ref()` / `is_some()` 都是方法，不是判据字段）。
 */
function fieldsRead(body: string): Set<string> {
  const out = new Set<string>();
  for (const m of body.matchAll(/\??\.\s*([A-Za-z_][A-Za-z0-9_]*)\s*(\(?)/g)) {
    if (m[2] !== '(') out.add(m[1]);
  }
  return out;
}

/** `wireguard_settings` / `wireguardSettings` → 同一个 key，绕开 snake↔camel 的映射细节。 */
function canonical(keys: Set<string>): string[] {
  return [...keys].map((k) => k.replace(/_/g, '').toLowerCase()).sort();
}

const TS_IS_WARP = fnBody(TS_WARP, 'export function isWarpServer(', '前端 isWarpServer');
const RUST_IS_WARP = fnBody(RUST_WARP, 'pub fn is_warp_server(', 'Rust is_warp_server');
const TS_MESH_SYSTEM = fnBody(
  TS_ENDPOINT_ROUTES,
  'export function meshUsesSystemInterface(',
  '前端 meshUsesSystemInterface'
);
const RUST_MESH_SYSTEM = fnBody(
  RUST_ENDPOINT_ROUTES,
  'pub fn mesh_uses_system_interface(',
  'Rust mesh_uses_system_interface'
);

describe('解析器自检（解析不到必须自曝，不得让后面的断言恒真）', () => {
  /**
   * 判据是「函数体里有字段读」而不是「有 return」（第一版写的是后者，变异实测踩中）：
   * Rust 的 `mesh_uses_system_interface` 主体是表达式位置的 `match`，**一个 `return` 都没有**——
   * 拿 `return` 当自检等于把「解析失败」和「合法重构」混成同一个红。
   *
   * 「有字段读」则直接钉住本提取器唯一的真实陷阱：`isWarpServer` 的形参是内联对象类型
   * （`server: { protocol?: string; … }`），取错块就会取到那段类型体 —— 而类型体里
   * 只有 `protocol?: string;` 这种声明、**没有 `.protocol` 这种读**，`fieldsRead` 恒空。
   */
  it('四个函数体都非空、且解析出的确实是代码块（非形参类型体）', () => {
    for (const [label, body] of [
      ['前端 isWarpServer', TS_IS_WARP],
      ['Rust is_warp_server', RUST_IS_WARP],
      ['前端 meshUsesSystemInterface', TS_MESH_SYSTEM],
      ['Rust mesh_uses_system_interface', RUST_MESH_SYSTEM],
    ] as const) {
      expect(body.trim().length, `${label} 函数体解析为空`).toBeGreaterThan(20);
      expect(
        fieldsRead(body).size,
        `${label} 解析出的块里一个字段读都没有 —— 多半取到了形参类型体`
      ).toBeGreaterThan(0);
    }
  });

  it('剔注释确实生效（否则锁 5 会被注释盖绿）', () => {
    // 两侧 meshUsesSystemInterface 的注释里都写着 WARP 谓词名；剔干净后仍能匹配到才是真的接线。
    expect(stripComments('// isWarpServer(server)\nfoo;')).not.toMatch(/isWarpServer/);
    expect(stripComments('/* is_warp_server */ bar;')).not.toMatch(/is_warp_server/);
  });
});

describe('锁 2：WARP 端点域名常量单一真值', () => {
  it('前端与 Rust 的 WARP_ENDPOINT_DOMAIN 逐字相等', () => {
    const m = /pub const WARP_ENDPOINT_DOMAIN:\s*&str\s*=\s*"([^"]+)"/.exec(
      stripComments(RUST_WARP)
    );
    expect(m, 'Rust 侧 WARP_ENDPOINT_DOMAIN 解析失败（改名/移走了？）').not.toBeNull();
    expect(m![1]).toBe(WARP_ENDPOINT_DOMAIN);
  });

  it('前端提示值与 Rust 的 WARP_MTU 逐字相等', () => {
    const m = /pub const WARP_MTU:\s*u32\s*=\s*(\d+)/.exec(stripComments(RUST_WARP));
    expect(m, 'Rust 侧 WARP_MTU 解析失败（改名/移走了？）').not.toBeNull();
    expect(Number(m![1])).toBe(WARP_MTU);
  });

  /**
   * `polaris-mesh` 必须 re-export config-engine 那份，不得自己再写一遍字面量。
   * 牙：在 `crates/mesh/src/warp.rs` 恢复 `pub const WARP_ENDPOINT_DOMAIN: &str = "…";` → 转红。
   */
  it('crates/mesh 不重新定义该常量（re-export，不是第二份字面量）', () => {
    const meshSrc = stripComments(RUST_MESH_WARP);
    expect(meshSrc).not.toMatch(/pub const WARP_ENDPOINT_DOMAIN\s*:/);
    expect(meshSrc).not.toMatch(/pub const WARP_MTU\s*:/);
    expect(meshSrc, 'mesh 侧应 re-export config-engine 的常量').toMatch(
      /pub use polaris_config_engine::warp::WARP_ENDPOINT_DOMAIN\s*;/
    );
  });
});

describe('锁 3/4：WARP 判据两侧同源', () => {
  /**
   * 牙：删掉 Rust 的 `warpDevice` 那条（或前端的），字段集不等 → 转红。
   * 反向同理：单侧新增一个判据字段（比如改看 `providerName`）也红。
   */
  it('两侧谓词读的 ServerConfig 字段集逐项相等', () => {
    expect(canonical(fieldsRead(RUST_IS_WARP))).toEqual(canonical(fieldsRead(TS_IS_WARP)));
  });

  it('字段集就是 protocol / wireguardSettings / warpDevice / address', () => {
    // 钉住当前形态：任一侧单独重构成别的字段组合，锁 3 也许仍相等（两侧一起改），本条兜住。
    expect(canonical(fieldsRead(TS_IS_WARP))).toEqual([
      'address',
      'protocol',
      'warpdevice',
      'wireguardsettings',
    ]);
  });

  /**
   * 域名兜底腿不得单侧消失 —— 旧 / 导入 / 上游 迁移来的 WARP **没有** `warpDevice` 标记，
   * 只认标记的话，恰好是会撞 `resource busy` FATAL 的那批节点漏判。
   */
  it('两侧谓词都引用 WARP_ENDPOINT_DOMAIN（域名兜底）', () => {
    expect(TS_IS_WARP).toMatch(/WARP_ENDPOINT_DOMAIN/);
    expect(RUST_IS_WARP).toMatch(/WARP_ENDPOINT_DOMAIN/);
  });
});

describe('锁 5：system 否决接线两侧都在（本缺陷的直接对应锁）', () => {
  /** 牙：把 Rust `mesh_uses_system_interface` 里的 `is_warp_server` 判断删掉 → 转红（缺陷复发即红）。 */
  it('Rust mesh_uses_system_interface 调用 is_warp_server', () => {
    expect(
      RUST_MESH_SYSTEM,
      'Rust 侧 WARP 否决消失 ⇒ 导入/手改/迁移来的 WARP 节点会发 system:true ⇒ resource busy FATAL'
    ).toMatch(/is_warp_server\s*\(/);
  });

  it('前端 meshUsesSystemInterface 调用 isWarpServer', () => {
    expect(TS_MESH_SYSTEM).toMatch(/isWarpServer\s*\(/);
  });
});
