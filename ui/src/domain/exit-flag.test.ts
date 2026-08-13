import { describe, expect, it } from 'vitest';
import {
  resolveExitCountryCode,
  resolveExitNodeFlagCode,
  resolveExitRegion,
  localizeRegion,
} from './exit-flag';

describe('resolveExitCountryCode', () => {
  /** 已连接只认 proxy 腿：这是「我从哪出去」的唯一真值。 */
  it('已连接时取代理出口的地区码', () => {
    expect(resolveExitCountryCode(true, 'HK', 'CN')).toBe('HK');
  });

  /** 未连接只认 direct 腿：此时本机出口就是真出口。 */
  it('未连接时取直连出口的地区码', () => {
    expect(resolveExitCountryCode(false, 'HK', 'CN')).toBe('CN');
  });

  /**
   * 🔴 **不跨连接态回落**（变异锁：写成 `proxy ?? direct` → 本条转红）。
   * 已连接但代理出口还没探到时回落 direct，等于把**本机**所在地当成「出口」展示 —— 用户会据此
   * 以为分流没生效 / 节点在国内。留空才诚实。
   */
  it('已连接但代理腿未探到时留空，绝不回落到直连腿', () => {
    expect(resolveExitCountryCode(true, undefined, 'CN')).toBeNull();
    expect(resolveExitCountryCode(true, null, 'CN')).toBeNull();
  });

  /** 反向同理：未连接时代理腿的残值（上一次连接留下的）不得冒充当前出口。 */
  it('未连接时不吃代理腿的残值', () => {
    expect(resolveExitCountryCode(false, 'HK', undefined)).toBeNull();
  });

  /**
   * 空串 = 探到了 IP 但对端没给地区（Cloudflare trace 偶发）。必须与 `null` 同归「留空」，
   * 否则会走到 `countryCodeToFlagAsset('')` 这类边界上去。
   */
  it('空串视同未探到', () => {
    expect(resolveExitCountryCode(true, '', undefined)).toBeNull();
    expect(resolveExitCountryCode(false, undefined, '')).toBeNull();
  });

  /** 两腿皆无（冷启动、探测尚未跑）→ 留空。**不画地球占位**（见模块头注）。 */
  it('冷启动两腿皆空时留空', () => {
    expect(resolveExitCountryCode(true, undefined, undefined)).toBeNull();
    expect(resolveExitCountryCode(false, undefined, undefined)).toBeNull();
  });
});

describe('resolveExitNodeFlagCode（首页「出口节点」框）', () => {
  /**
   * 🔴 **回归**：断开态绝不许画旗。
   *
   * 缺陷长相：本框把旗面与 `currentServer.name` + `currentServer.address` 画在同一行。断开时若沿用
   * `resolveExitCountryCode`（未连接取 direct 腿 = **本机**地区），界面渲染成
   * 「HK03 / hk03.example.com:443 🇨🇳」——用户只会读成**「这个节点在中国」**，而 CN 是本机所在地、
   * 与那个节点毫无关系。这是「用非出口数据冒充出口」的又一变体。
   *
   * **变异锁**：把实现退回 `resolveExitCountryCode(connected, proxyCC, directCC)`（即旧逻辑）→ 本条转红。
   */
  it('未连接时留空，绝不把本机地区旗画在代理节点名旁', () => {
    expect(resolveExitNodeFlagCode(false, undefined)).toBeNull();
    // 断开态即便 direct 腿已探到本机在 CN，本框也必须留空（状态栏才是该显示它的地方）。
    expect(resolveExitCountryCode(false, undefined, 'CN')).toBe('CN'); // 状态栏口径：照显
    expect(resolveExitNodeFlagCode(false, undefined)).toBeNull(); // 出口框口径：留空
  });

  /** 断开态下代理腿的残值（上一次连接留下的）同样不得冒充当前出口。 */
  it('未连接时不吃代理腿残值', () => {
    expect(resolveExitNodeFlagCode(false, 'HK')).toBeNull();
  });

  /** 已连接且代理出口已探到 → 正常显示（否则这道闸就成了「永远不画」的死规则）。 */
  it('已连接时显示代理出口地区码', () => {
    expect(resolveExitNodeFlagCode(true, 'HK')).toBe('HK');
  });

  /** 已连接但代理腿未探到 / 无地区 → 留空，不回落本机（与状态栏同一条「不跨态回落」纪律）。 */
  it('已连接但代理腿未探到时留空', () => {
    expect(resolveExitNodeFlagCode(true, undefined)).toBeNull();
    expect(resolveExitNodeFlagCode(true, null)).toBeNull();
    expect(resolveExitNodeFlagCode(true, '')).toBeNull();
  });
});

describe('resolveExitRegion（旗优先，无旗回落地名文本）', () => {
  // 'us'/'cn' 在旗面资产集（flag-assets.generated.ts）内；'zw' 是合法 ISO 但**无**旗面资产 → 画不出旗。

  /** ① 有 countryCode（能画旗）→ flag。 */
  it('有能画旗的 countryCode → flag', () => {
    expect(resolveExitRegion(true, { countryCode: 'US' }, null)).toEqual({ kind: 'flag', code: 'US' });
    // 未连接取 direct 腿：境内直连 ipip 派生 cn → 仍显旗（保留既有境内行为，不退化成冗长地名）。
    expect(resolveExitRegion(false, null, { country: '中国 北京 北京 电信', countryCode: 'cn' })).toEqual({
      kind: 'flag',
      code: 'cn',
    });
  });

  /** ② 无 countryCode 但有 country（境外直连出口 ipip 无 ISO）→ text（原始地名，未本地化）。 */
  it('无 countryCode 有 country → text 地名回落', () => {
    expect(resolveExitRegion(false, null, { country: '美国 加利福尼亚' })).toEqual({
      kind: 'text',
      region: '美国 加利福尼亚',
    });
  });

  /** ③ 两者皆无 → none（调用方留空：现状裸 IP / 空）。 */
  it('两者皆无 → none', () => {
    expect(resolveExitRegion(false, null, {})).toEqual({ kind: 'none' });
    expect(resolveExitRegion(true, null, { country: 'x' })).toEqual({ kind: 'none' }); // 连接态只认 proxy 腿
    expect(resolveExitRegion(true, undefined, undefined)).toEqual({ kind: 'none' });
  });

  /**
   * 🔴 **flag-drawable 闸**（非仅「countryCode 是否存在」）：有 ISO 码但无旗面资产（如 zw）→ 不能显旗。
   * 此时若有 country 地名 → 回落 country；无 country → 回落该码本身（交 localizeRegion 折成地区名）。
   * 变异锁：把闸写成「code 非空即 flag」→ 会对 zw 返 flag（FlagImg 实际画 null）→ 本条转红。
   */
  it('有 ISO 码但无旗面资产 → 回落 text（country 优先，否则回落码本身）', () => {
    expect(resolveExitRegion(false, null, { country: '津巴布韦', countryCode: 'ZW' })).toEqual({
      kind: 'text',
      region: '津巴布韦', // country ?? countryCode 顺序：country 优先
    });
    expect(resolveExitRegion(false, null, { countryCode: 'ZW' })).toEqual({
      kind: 'text',
      region: 'ZW', // 无 country → 回落码本身（localizeRegion 会折成地区名）
    });
  });

  /**
   * 🔴 **不跨连接态回落**（与 resolveExitCountryCode 同纪律）：连接态代理腿未探到时，绝不吃 direct 腿。
   * 变异锁：`connected ? proxy : direct` 写成 `proxy ?? direct` → 本条转红。
   */
  it('连接态代理腿未探到时不回落 direct 腿', () => {
    expect(resolveExitRegion(true, null, { country: '美国 加利福尼亚', countryCode: 'US' })).toEqual({
      kind: 'none',
    });
  });

  /** 空串（探到 IP 但对端没给地区）与 null/undefined 同归未探到，不落入 countryCodeToFlagAsset('') 边界。 */
  it('空串视同未探到', () => {
    expect(resolveExitRegion(false, null, { country: '', countryCode: '' })).toEqual({ kind: 'none' });
  });
});

describe('localizeRegion（1:1 移植 上游）', () => {
  /** 2 位 ISO 码 → 本地化地区名（用 en 稳定断言，避免依赖运行环境默认语言）。 */
  it('2 位 ISO 码折成地区名', () => {
    expect(localizeRegion('US', 'en')).toBe('United States');
    // 大小写不敏感（小写 hk 也被 uppercase 后解析）；ICU 各版本对 HK 的措辞不同（Hong Kong / Hong Kong SAR China），
    // 只钉「解析成了地区名且以 Hong Kong 起头」，不钉 ICU 具体措辞。
    expect(localizeRegion('hk', 'en')).toMatch(/^Hong Kong/);
  });

  /** 非 2 位码（ipip 地名文本 / 城市）原样返回——不误当国家码去 DisplayNames。 */
  it('非 2 位码原样返回', () => {
    expect(localizeRegion('美国 加利福尼亚', 'en')).toBe('美国 加利福尼亚');
    expect(localizeRegion('USA', 'en')).toBe('USA'); // 3 字母不匹配
  });

  /** 空值 → undefined（调用方据此不渲染地区槽位）。 */
  it('空值 → undefined', () => {
    expect(localizeRegion(undefined, 'en')).toBeUndefined();
    expect(localizeRegion(null, 'en')).toBeUndefined();
    expect(localizeRegion('', 'en')).toBeUndefined();
  });
});
