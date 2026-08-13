/**
 * 出口旗面的**唯一**数据源判定：出口 IP 探测回来的 `countryCode`。
 *
 * # 为什么单独一个模块，而不是复用 `flag-detect.ts`
 *
 * 本仓有**两种**旗面语义，它们的数据源不同、不可互换，混用是本轮真机反馈的根子：
 *
 * | 位置 | 数据源 | 语义 |
 * |---|---|---|
 * | 状态栏 / 首页「出口节点」框 | **出口 IP 探测的 countryCode**（本模块） | 「我现在**从哪出去**」 |
 * | 节点列表 `NodeCard` / 首页节点选单 | 名称派生（`flag-detect.getCountryCode`） | 「这个节点**自称**在哪」 |
 *
 * 🔴 **名称 / 入口域名派生的地区，绝不允许用来表达「出口在哪」**（状态栏、出口节点框）：
 * `hk03.example.com` 只说明「连到哪」，中转链下真实落地可能在别处 —— 用入口冒充出口，**比不画旗更糟**。
 * 反之名称派生用在列表标签上并没有说谎（它回答的本就是「自称」），故列表维持现状、不做「已校正/未校正」
 * 两种视觉（同一符号两种含义需要用户学习区分规则，得不偿失）。
 *
 * # 未探测 → 留空，不画地球占位
 *
 * 探测未回来 / 探到但无 countryCode / 该地区无旗面资产 —— 一律返回 `null`，调用方**什么都不画**。
 * 地球占位会被读成「出口在某个未知国家」，而真相是「还不知道」；空位至少不说谎。
 *
 * # 不跨连接态回落
 *
 * 与出口 IP 文本同一条口径（见 `layout/status-bar-display.ts` 第 3 条）：已连接只认 proxy 腿、
 * 未连接只认 direct 腿。回落会在代理探测未就绪时把**本机**所在地当成「出口」展示。
 *
 * # 无旗回落地名文本（消费 ipip 的 `country` 真值）
 *
 * 境内直连出口 ipip 能派生 ISO 码（cn/hk/mo/tw）→ 有旗；**境外**直连出口 ipip 库无 ISO 码
 * （`countryCode=None`）→ 画不出旗，此前只剩裸 IP。[`resolveExitRegion`] 在无旗时回落到 ipip 探到的
 * `country` 地名文本（真实探测出的地区，非节点名派生 —— 符合「出口位置用真值」）。见 [`localizeRegion`]。
 */
import { countryCodeToFlagAsset } from './flag-assets';

/**
 * 出口地区码（alpha-2，小写化交由 `countryCodeToFlagAsset` 入口处理）。
 *
 * @param connected 核是否在跑（决定认哪条腿，**不回落**）
 * @param proxyCountryCode 代理出口探测到的地区码
 * @param directCountryCode 本地直连出口探测到的地区码
 * @returns 地区码；未探测 / 无地区信息 → `null`（调用方留空）
 */
export function resolveExitCountryCode(
  connected: boolean,
  proxyCountryCode: string | null | undefined,
  directCountryCode: string | null | undefined
): string | null {
  const code = connected ? proxyCountryCode : directCountryCode;
  return code ? code : null;
}

/** 出口地区展示的三态判定结果（[`resolveExitRegion`] 产出）。 */
export type ExitRegionDisplay =
  | { kind: 'flag'; code: string } // 能画旗：countryCode 解析出旗面资产 → 调用方显旗。
  | { kind: 'text'; region: string } // 无旗有地名：回落**未本地化**的原始 region（经 `localizeRegion` 后显示）。
  | { kind: 'none' }; // 两者皆无：调用方留空（现状——裸 IP / 空，不画地球占位）。

interface ExitLeg {
  country?: string | null;
  countryCode?: string | null;
}

/**
 * 出口地区展示决策（**旗优先，无旗回落地名文本**）——消费 ipip 对境外返回的 `country` 真值，
 * 让境外直连出口不再只剩裸 IP。
 *
 * 1:1 对齐 上游 home-status-bar `country ?? countryCode` 的回落**顺序**，但 Polaris 是旗优先：
 *  - `countryCode` 能画旗（解析出旗面资产）→ `flag`（境内 cn/hk/mo/tw、代理腿 cloudflare ISO 均走此支）；
 *  - 画不出旗（境外 ipip 无 ISO / 该地区无旗面资产），退到 `country ?? countryCode`：
 *    有 `country`（ipip 地名文本）→ `text`；否则退到未画成旗的 `countryCode`（经 `localizeRegion` 折成地区名）；
 *  - 两者皆无 → `none`。
 *
 * **不跨连接态回落**（与 [`resolveExitCountryCode`] 同口径）：已连接只认 proxy 腿、未连接只认 direct 腿。
 * `text` 分支的 `region` **未本地化**（保持本函数 i18n 无关、可 node 直测）；调用方经 [`localizeRegion`]
 * 折成当前语言地区名后再显示。空串（探到 IP 但对端没给地区）与 null/undefined 同归「未探到」。
 *
 * @param connected 核是否在跑（决定认哪条腿，**不回落**）
 * @param proxy 代理出口探测信息（cloudflare trace：有 `countryCode`、无 `country`）
 * @param direct 本地直连出口探测信息（ipip：境内有 ISO；境外仅 `country` 地名、无 ISO）
 */
export function resolveExitRegion(
  connected: boolean,
  proxy: ExitLeg | null | undefined,
  direct: ExitLeg | null | undefined
): ExitRegionDisplay {
  const leg = connected ? proxy : direct;
  const code = leg?.countryCode || undefined; // 空串视同未探到
  if (code && countryCodeToFlagAsset(code)) return { kind: 'flag', code };
  const region = (leg?.country || undefined) ?? code; // country ?? countryCode（对齐 上游 顺序）
  return region ? { kind: 'text', region } : { kind: 'none' };
}

/**
 * ISO alpha-2 地区码 → 当前语言地区名（`HK`→香港 / Hong Kong，随 i18n）；**非** 2 位码
 * （ipip 地名文本 / 城市）原样返回。1:1 移植 上游 home-status-bar `localizeRegion`。
 *
 * 用途：[`resolveExitRegion`] 的 `text` 分支产出的 `region` 可能是 ipip 地名文本（原样）或未画成旗的
 * 2 位 ISO 码（这里折成可读地区名，而非把裸 `US` 展示给用户）。`Intl.DisplayNames` 不可用时（极旧
 * runtime）回落原值，绝不抛。
 */
export function localizeRegion(
  v: string | null | undefined,
  lang: string
): string | undefined {
  if (!v) return undefined;
  if (/^[A-Za-z]{2}$/.test(v)) {
    try {
      return new Intl.DisplayNames([lang], { type: 'region' }).of(v.toUpperCase()) ?? v;
    } catch {
      return v;
    }
  }
  return v;
}

/**
 * 首页「出口节点」框专用：**未连接一律留空**，比 [`resolveExitCountryCode`] 多一道闸。
 *
 * # 为什么这一处比状态栏更严
 *
 * 两处数据源相同，但**同屏渲染的邻居**不同：
 *
 * - 状态栏里旗面与出口 IP 同处 `.sb-fold-ip` 组、与节点名分属不同 span ⇒ 旗面显然是在修饰 IP，
 *   断开态画本机地区旗读作「我现在从本机出去」，正确。
 * - 出口节点框里旗面紧贴 `currentServer.name` + `currentServer.address` ⇒ 断开态会渲染成
 *   「HK03 / hk03.example.com:443 🇨🇳」，用户只会读成**「这个节点在中国」**。而 CN 是**本机**所在地，
 *   与那个节点毫无关系。
 *
 * 这就是「用非出口数据冒充出口」的又一变体 —— 与 §11.4-1 否掉「入口域名派生出口旗」同一类错误，
 * 只不过这次冒充者是本机直连腿而不是入口域名。未连接时本就不存在「代理出口」，留空才诚实。
 *
 * @param connected 核是否在跑
 * @param proxyCountryCode 代理出口探测到的地区码
 * @returns 地区码；未连接 / 未探到 → `null`（调用方整个槽位不渲染）
 */
export function resolveExitNodeFlagCode(
  connected: boolean,
  proxyCountryCode: string | null | undefined
): string | null {
  if (!connected) return null;
  return resolveExitCountryCode(true, proxyCountryCode, undefined);
}
