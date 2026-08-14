/**
 * 规则资源库（catalog）—— **前端不再持有清单本体**。
 *
 * 内置清单（28 条 = 随包表 `builtin_geo_rulesets()` 的投影）的单一真值在 Rust
 * （`crates/config-engine/src/user_config/rule_resource_catalog.rs`），经 `rule_resources_get_catalog`
 * / `rule_resources_refresh_catalog` 下发（`RuleResourceCatalogResult`，`source` 字段自述来源）。
 * 依治理文档 §3.1 Q3：**常量表 → Rust SoT + 一次 invoke 拉取入 store**。
 * 迁移当时已逐条对拍取证：33/33 全等（category/name/path 派生），并对 Rust 表变异验证转红
 * ——「33」是当时两侧共有的条数，其后 Rust 侧收敛成随包表的投影（28 条，81a4e68）。
 *
 * `MRD_RAW_BASE` / `mrdRawUrl` / `RULE_RESOURCE_CATALOG` / `findCatalogItem` 一并删除：
 * 下载 URL 的拼装发生在**下载那一侧**（Rust），前端不下载，留着就是第二份基址常量。
 *
 * 本模块只剩 `deriveResourceMeta`（②类 UI 派生：手动 URL 输入时的**即时预填**，同步渲染路径）。
 * 审计 §B 记它为「资源库弹窗」的待接线物料，弹窗未实现故暂无消费方 —— 按 §B 判据保留，勿当死代码删。
 *
 * ⚠️ 已登记漂移敞口：将来 Rust 侧接下载时会需要一份等价的「URL → category/name」兜底命名
 * （原注释即写明「主进程兜底与渲染端预填共用」）。届时应以 Rust 为准、前端谓词退役，
 * **不要两侧各留一份**（那正是本批消灭的形态）。
 */
import type { RuleResourceCategory } from '../contracts/types';

/** 分类显示名：技术类别固定显示 Geosite/GeoIP；custom 文案由调用方按当前 locale 注入。 */
export function categoryLabel(cat: RuleResourceCategory, customLabel: string): string {
  switch (cat) {
    case 'geosite':
    case 'geosite-lite':
      return 'Geosite';
    case 'geoip':
    case 'geoip-lite':
      return 'GeoIP';
    default:
      return customLabel;
  }
}

/**
 * 资源库条目的「已具备」状态：随包出厂 / 已下载 / 都不是（= 真正可下载）。
 *
 * 抽成纯函数而非写在弹窗里：本仓 vitest 是 node 环境无 jsdom，组件交互测不了，判据留在组件里
 * 等于没有门。而这条判据错一次的代价是实打实的 —— 判漏 → 用户对着已在盘上的资源反复下载；
 * 判多 → 用户以为规则集在手，实际生成配置时该 tag 无处可寻（fail-closed 剪枝，规则静默失效）。
 *
 * **随包优先于已下载**（两者同真时返 `'bundled'`）：`route.rs` 生成 rule_set 时先注入随包副本、
 * 再对未定义的 tag 才去查 `config.ruleResources`，即随包在位时**生效的恒是随包那份**，下载副本
 * 只在随包 `.srs` 缺失/损坏时才顶上。此时标「已下载」会让用户以为生效的是自己下的那份。
 */
export type CatalogItemStatus = 'bundled' | 'downloaded' | null;

export function catalogItemStatus(
  item: { id: string; bundled?: boolean },
  downloadedIds: ReadonlySet<string>,
): CatalogItemStatus {
  if (item.bundled) return 'bundled';
  return downloadedIds.has(item.id) ? 'downloaded' : null;
}

/** 资源库条目排序/去重只需要这三个字段（组件传的是完整 `RuleResourceCatalogItem`）。 */
type CatalogSortable = { id: string; name: string; bundled?: boolean };

/**
 * 某个 tab 该显示哪些条目、按什么序 —— 「外置去重」+「已具备优先、名称次之」两件事的唯一落点。
 *
 * ## 去重：外置排掉与内置 id 重合的条目
 *
 * 判据是代码事实不是偏好：`crates/config-engine/src/builder/route.rs` 的 `add_local_geo_rule_set`
 * 在同 id 时**优先注入随包那份**，即外置清单里与内置重合的条目下载了也用不上（纯白下）。
 * 列出来只会让用户以为自己缺了什么。
 *
 * 实测重合面（2026-07-30，盘上缓存 `<userData>/rule-resource/catalog.json` 对随包 28 条）：
 * 外置 2176 条里重合 **27** 条 —— 唯一没重合的随包项是 `geosite-category-ai`（上游文件叫
 * `category-ai-!cn.srs` ⇒ 外置侧 id 是 `geosite-category-ai-!cn`，与随包 tag 不同形，见
 * `builtin_geo_rulesets.rs` 的 `app_geo_entry`）。⇒ 过滤后条数 2176 → 2149，量级上不显著，
 * 但那 27 条恰恰是最常被点的（cn / youtube / telegram / netflix…），去重收益集中在这里。
 *
 * ## 排序：已具备 > 名称
 *
 * ⚠️ 内置 tab 恒 `bundled:true`（`81a4e68` 把它收敛成随包表的投影）⇒ 该 tab 上第一级键恒相等、
 * 只剩名称排序。**这不是排序没生效**，是那次收敛的必然结果；实际因状态键改观的只有外置 tab。
 *
 * 名称同名（`geosite-netflix` / `geoip-netflix` 的 name 都是 `netflix`）时不再加第三级键：
 * `Array.prototype.sort` 自 ES2019 起保证稳定，同名项保持入参序即可，多一级键只是多一处要维护的判据。
 */
export function catalogTabItems<T extends CatalogSortable>(
  tab: 'builtin' | 'external',
  builtin: readonly T[],
  external: readonly T[],
  downloadedIds: ReadonlySet<string>,
): T[] {
  let src: readonly T[] = builtin;
  if (tab === 'external') {
    const builtinIds = new Set(builtin.map((it) => it.id));
    src = external.filter((it) => !builtinIds.has(it.id));
  }
  // 已具备(0) 在前、待下载(1) 在后；同档按名称。两级键缺任一级都会让排序退化成另一半。
  const held = (it: T) => (catalogItemStatus(it, downloadedIds) === null ? 1 : 0);
  return [...src].sort((a, b) => held(a) - held(b) || a.name.localeCompare(b.name));
}

/**
 * 资源库列表区一行都渲染不出来时该说什么 —— 这是一个**解释正确性**的判据，四态各有排他理由。
 *
 * 顺序不是排版偏好，每一条都在挡一句谎话：
 *  1. `count > 0` → `null`：有行就不是空态。**刷新失败但上一份缓存还在时不许拿失败态把清单顶掉**
 *     —— 用户点刷新前看得见的东西，点完反而没了，比不给反馈更糟。那一路的持久解释挂在
 *     外置状态行（`extStatusText`）上，不在这里。
 *  2. `total > 0` → `noMatch`：清单在手、只是被搜索过滤空了。此时报「加载失败」是反过来说谎。
 *  3. `error` → `error`：清单没拿到。这一条必须压过下面的 `noMatch` —— 加载失败时 `count` 同样是 0，
 *     不排在前面的话用户看到「无匹配资源」，而他根本没搜任何东西。**这就是本次要修的那句谎话。**
 *  4. `notFetched` → `notFetched`：外置 tab 一次都没拉过（既没预载到缓存、也没点过刷新）。
 *
 * 抽成纯函数的理由同 `catalogItemStatus`：本仓 vitest 无 jsdom，判据写在 JSX 三元里等于没有门。
 */
export type CatalogEmptyKind = 'error' | 'notFetched' | 'noMatch' | null;

export function catalogEmptyKind(p: {
  /** 当前 tab 的加载失败原因；`null` = 没出错。空串也算出错（后端未必给得出 message）。 */
  error: string | null;
  /** 外置 tab 且一次清单都没拿到。 */
  notFetched: boolean;
  /** 本 tab 去重排序后的总条数（**未经**搜索过滤）。 */
  total: number;
  /** 经搜索过滤后可渲染的行数。 */
  count: number;
}): CatalogEmptyKind {
  if (p.count > 0) return null;
  if (p.total > 0) return 'noMatch';
  if (p.error !== null) return 'error';
  if (p.notFetched) return 'notFetched';
  return 'noMatch';
}

/** 从下载 URL 推导资源 category 与默认 name（手动 URL 自动命名，渲染端预填）。 */
export function deriveResourceMeta(url: string): { category: RuleResourceCategory; name: string } {
  let pathname = url;
  try {
    pathname = new URL(url).pathname;
  } catch {
    pathname = url.split('?')[0].split('#')[0];
  }
  const segs = pathname.split('/').filter(Boolean);
  let base = segs.length ? segs[segs.length - 1] : 'rule';
  try {
    base = decodeURIComponent(base);
  } catch {
    /* keep */
  }
  base = base.replace(/\.srs$/i, '').replace(/\.json$/i, '') || 'rule';

  // meta-rules-dat 路径 → 对应分类（含 -lite），name 复用 basename（id 与内置同 ⇒ 自然去重）
  const m = pathname.match(/\/(geo|geo-lite)\/(geosite|geoip)\//i);
  if (m) {
    const lite = m[1].toLowerCase() === 'geo-lite';
    const kind = m[2].toLowerCase(); // geosite | geoip
    const category = (lite ? `${kind}-lite` : kind) as RuleResourceCategory;
    return { category, name: base };
  }

  // asn → AS<number>
  const asn = pathname.match(/\/asn\/(AS\d+)\.srs$/i);
  if (asn) return { category: 'custom', name: asn[1].toUpperCase() };

  return { category: 'custom', name: base.replace(/[^\w.-]+/g, '_') };
}
