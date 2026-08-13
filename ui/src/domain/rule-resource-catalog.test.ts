/**
 * 资源库条目「已具备」判据 —— 真机 2026-07-30 反馈：随包资源在资源库里显示成待下载，
 * 已下载的也不勾选，用户无从分辨哪些还需要下。
 *
 * 组件本身在 node 环境测不了（无 jsdom），故判据留在纯函数里由本文件钉死。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { catalogItemStatus, catalogTabItems, catalogEmptyKind } from './rule-resource-catalog';

const none: ReadonlySet<string> = new Set();

describe('catalogItemStatus', () => {
  it('随包出厂 → bundled（显示「已内置」，不再作为下载目标）', () => {
    expect(catalogItemStatus({ id: 'geosite-youtube', bundled: true }, none)).toBe('bundled');
  });

  it('已下载到本地 → downloaded', () => {
    expect(catalogItemStatus({ id: 'geoip-us', bundled: false }, new Set(['geoip-us']))).toBe(
      'downloaded',
    );
  });

  it('随包 + 已下载同真 → bundled 优先：route.rs 里生效的恒是随包那份，标「已下载」会误导', () => {
    expect(
      catalogItemStatus({ id: 'geosite-cn', bundled: true }, new Set(['geosite-cn'])),
    ).toBe('bundled');
  });

  it('两者都不是 → null，即真正需要下载的条目', () => {
    expect(catalogItemStatus({ id: 'geosite-apple', bundled: false }, new Set(['geoip-us']))).toBe(
      null,
    );
  });

  it('bundled 缺失（后端未下发该字段）→ 按未随包处理：宁可让用户多下一次，也不谎称已在手', () => {
    expect(catalogItemStatus({ id: 'geosite-apple' }, none)).toBe(null);
    expect(catalogItemStatus({ id: 'geosite-apple' }, new Set(['geosite-apple']))).toBe(
      'downloaded',
    );
  });
});

// ---------------------------------------------------------------------------
// 排序：已具备 > 名称。两级键**缺任一级**都必须转红。
// ---------------------------------------------------------------------------
describe('catalogTabItems 排序键完整性', () => {
  it('已下载的排到未具备的前面，哪怕名称在后 —— 变异「只按名称排」在这里转红', () => {
    const ext = [
      { id: 'geosite-alpha', name: 'alpha' },
      { id: 'geosite-zulu', name: 'zulu' },
    ];
    expect(
      catalogTabItems('external', [], ext, new Set(['geosite-zulu'])).map((i) => i.id),
      '状态键没生效：已下载的 zulu 被名称序压在 alpha 后面',
    ).toEqual(['geosite-zulu', 'geosite-alpha']);
  });

  it('同为未具备时按名称升序 —— 入参刻意逆序，变异「只按状态排」在稳定排序下原样返回即转红', () => {
    const ext = [
      { id: 'geosite-zulu', name: 'zulu' },
      { id: 'geosite-alpha', name: 'alpha' },
    ];
    expect(
      catalogTabItems('external', [], ext, none).map((i) => i.id),
      '名称键没生效：同档条目保持了入参序',
    ).toEqual(['geosite-alpha', 'geosite-zulu']);
  });

  it('随包与已下载同属「已具备」一档，档内仍按名称（不再各成一档）', () => {
    const ext = [
      { id: 'geosite-zulu', name: 'zulu', bundled: true },
      { id: 'geosite-alpha', name: 'alpha' },
    ];
    expect(
      catalogTabItems('external', [], ext, new Set(['geosite-alpha'])).map((i) => i.id),
    ).toEqual(['geosite-alpha', 'geosite-zulu']);
  });

  it('内置 tab 恒 bundled ⇒ 第一级键恒相等、只剩名称序（这是 81a4e68 收敛的必然结果，不是排序失灵）', () => {
    const bi = [
      { id: 'geosite-youtube', name: 'youtube', bundled: true },
      { id: 'geoip-cn', name: 'cn', bundled: true },
    ];
    expect(catalogTabItems('builtin', bi, [], none).map((i) => i.id)).toEqual([
      'geoip-cn',
      'geosite-youtube',
    ]);
  });

  it('不就地改入参：组件把 state 数组原样传进来，原地 sort 会绕过 React 的引用比对', () => {
    const bi = [
      { id: 'b', name: 'b', bundled: true },
      { id: 'a', name: 'a', bundled: true },
    ];
    catalogTabItems('builtin', bi, [], none);
    expect(bi.map((i) => i.id)).toEqual(['b', 'a']);
  });
});

// ---------------------------------------------------------------------------
// 去重：外置排掉与内置 id 重合的条目。
//
// 判据是代码事实：`builder/route.rs` 的 `add_local_geo_rule_set` 同 id 时优先注入随包那份
// ⇒ 重合条目下载了也用不上。实测重合面（2026-07-30，盘上缓存对随包 28 条）：外置 2176 条里
// 重合 27 条（唯一漏网的随包项是 `geosite-category-ai`，上游文件名带 `-!cn` ⇒ 外置侧 id 不同形）。
// ---------------------------------------------------------------------------
describe('catalogTabItems 外置去重', () => {
  const bi = [
    { id: 'geosite-cn', name: 'cn', bundled: true },
    { id: 'geosite-youtube', name: 'youtube', bundled: true },
  ];
  const ext = [
    { id: 'geosite-cn', name: 'cn' },
    { id: 'geosite-apple', name: 'apple' },
    { id: 'geosite-youtube', name: 'youtube' },
  ];

  it('与内置 id 重合的条目不出现在外置 tab —— 变异「不过滤内置 id」在这里转红', () => {
    expect(
      catalogTabItems('external', bi, ext, none).map((i) => i.id),
      '外置 tab 仍列着随包已有的条目：下载它们是纯白下（route.rs 恒采用随包那份）',
    ).toEqual(['geosite-apple']);
  });

  it('去重只按 id，不看 bundled 字段：外置那份可能没带 bundled，靠它过滤会漏干净', () => {
    expect(catalogTabItems('external', bi, ext, none).some((i) => i.id === 'geosite-cn')).toBe(
      false,
    );
  });

  it('内置 tab 不参与去重（否则内置 tab 会把自己清空）', () => {
    expect(catalogTabItems('builtin', bi, ext, none).map((i) => i.id)).toEqual([
      'geosite-cn',
      'geosite-youtube',
    ]);
  });

  it('内置清单还没加载（空数组）时不误杀外置条目 —— 宁可多列，不可凭空清空列表', () => {
    expect(catalogTabItems('external', [], ext, none).map((i) => i.id)).toEqual([
      'geosite-apple',
      'geosite-cn',
      'geosite-youtube',
    ]);
  });
});

// ---------------------------------------------------------------------------
// 空态：加载失败必须有**持久**解释，且不许被「无匹配资源」顶掉。
// ---------------------------------------------------------------------------
describe('catalogEmptyKind', () => {
  const base = { error: null as string | null, notFetched: false, total: 0, count: 0 };

  it('清单没拿到 + 列表空 → error（变异「加载失败时不渲染空态」在这里转红）', () => {
    expect(catalogEmptyKind({ ...base, error: 'ECONNREFUSED' })).toBe('error');
  });

  it('error 压过 noMatch：加载失败时 count 同样是 0，报「无匹配资源」是与事实相反的解释', () => {
    // 变异「把 error 分支挪到 noMatch 之后」⇒ 返回 noMatch ⇒ 红。
    expect(catalogEmptyKind({ ...base, error: 'boom' })).not.toBe('noMatch');
  });

  it('error 压过 notFetched：外置一次没拉到 + 刷新失败，原因是失败而不是「还没拉」', () => {
    expect(catalogEmptyKind({ ...base, error: 'boom', notFetched: true })).toBe('error');
  });

  it('空串也算失败（后端未必给得出 message），不得因此退回 noMatch', () => {
    expect(catalogEmptyKind({ ...base, error: '' })).toBe('error');
  });

  it('列表还有行时不进空态：刷新失败但上一份缓存还在，**不许**拿失败态把还能用的清单顶掉', () => {
    expect(catalogEmptyKind({ ...base, error: 'boom', total: 2176, count: 2176 })).toBe(null);
  });

  it('清单在手、只是被搜索过滤空了 → noMatch（此时报「加载失败」是反过来说谎）', () => {
    expect(catalogEmptyKind({ ...base, error: 'boom', total: 2176, count: 0 })).toBe('noMatch');
  });

  it('无失败时的三态：未拉过 → notFetched；拉到但空 → noMatch；有行 → null', () => {
    expect(catalogEmptyKind({ ...base, notFetched: true })).toBe('notFetched');
    expect(catalogEmptyKind({ ...base })).toBe('noMatch');
    expect(catalogEmptyKind({ ...base, total: 3, count: 3 })).toBe(null);
  });
});

// ---------------------------------------------------------------------------
// 接线：上面三条判据必须真的被弹窗消费。纯函数测不到 JSX，这里按源码结构钉住。
// 手段与 `lib/config-write-wiring.test.ts` / `dialogs/dialog-toast-layer.test.ts` 同源。
// ---------------------------------------------------------------------------
describe('ResCatalogDialog 接线', () => {
  const src = readFileSync(
    fileURLToPath(new URL('../components/dialogs/ResCatalogDialog.tsx', import.meta.url)),
    'utf8',
  )
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/^\s*\/\/.*$/gm, '');

  it('前提自检：读到的确实是那份源码（路径写错会让下面全部恒绿）', () => {
    expect(src).toMatch(/export function ResCatalogDialog/);
  });

  it('列表条目走 catalogTabItems —— 组件里另写一套或直接取 builtin/external 即转红', () => {
    expect(
      src,
      '排序与去重被绕开了：items 不再由 catalogTabItems 产出',
    ).toMatch(/const items = useMemo\(\s*\(\)\s*=>\s*catalogTabItems\(/);
  });

  it('空态判定走 catalogEmptyKind', () => {
    expect(src).toMatch(/catalogEmptyKind\(\{/);
  });

  it("列表区有 'error' 分支，且渲染「原因 + 重试」", () => {
    const m = src.match(/emptyKind === 'error' \? \(([\s\S]*?)\)\s*: emptyKind ===/);
    expect(
      m,
      "列表区没有 emptyKind === 'error' 分支 —— 清单加载失败又变回「空列表 + 零解释」",
    ).not.toBeNull();
    const jsx = m![1];
    expect(jsx, '失败空态缺标题').toMatch(/resCatalog\.loadFailed/);
    expect(jsx, '失败空态缺失败原因（只说「失败了」等于没说）').toMatch(/\{tabErr\}/);
    expect(jsx, '失败空态缺重试入口').toMatch(/common\.retry/);
    expect(jsx, '重试按钮没挂 onClick = 一个点不动的按钮').toMatch(/onClick=\{retryEmpty\}/);
  });

  it('加载/刷新失败落进 state 而不是 toast（toast 2.2s 就没了，非提交触发的失败必须持久）', () => {
    // 下载失败仍走 toast —— 那是提交触发（用户点了「下载选中」），注意力在，不在本门射程。
    expect(src, '初始加载失败没有落进持久 state').toMatch(/setLoadErr\(reason\(e\)\)/);
    expect(src, '刷新清单失败没有落进持久 state').toMatch(/setExtErr\(reason\(e\)\)/);
  });

  it('刷新失败时外置状态行也报失败：列表非空时空态不渲染，那一路只剩这条解释', () => {
    expect(src).toMatch(/if \(extErr !== null\) return t\('resCatalog\.loadFailed'/);
  });
});
