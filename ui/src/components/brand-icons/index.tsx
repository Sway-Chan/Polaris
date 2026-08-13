/**
 * 品牌图标解析器 —— 随包本地 SVG，解锁徽章 + 应用分流内置项共用（单一真值）。
 *
 * 图标本体是 `brand-svgs.ts` 里的已消毒 SVG 字符串（源自 homarr-labs/dashboard-icons，MIT）。
 * 本模块只做「canonical id → ReactNode」的解析：
 *  - 解锁服务 id（chatgpt/claude/gemini/grok/netflix/disney/tiktok/spotify）直接命中；
 *  - 应用分流预设 id 里 openai→chatgpt、anthropic→claude、twitter→x、epic→epic-games 走别名归一
 *    （预设表命名与品牌 canonical 名不同的场景）；youtube/tiktok/telegram/instagram/github/steam/
 *    google 预设 id 与 canonical key 同名，无需别名；riot 无 bundled 图标，回退分支处理；
 *  - 命中返回内联 SVG（`dangerouslySetInnerHTML`，内容为本地常量、非用户输入，无 XSS 面），未命中返回 null
 *    → 调用方回退（解锁徽章回退文字名；应用图标回退 emoji/首字母，不再触发远端 iconProxySrc）。
 */
import type { ReactNode } from 'react';
import { BRAND_SVGS, type BrandIconId } from './brand-svgs';

/** 应用分流预设 id → 解锁 canonical id 的别名（预设表历史命名差异）。 */
const ALIASES: Record<string, BrandIconId> = {
  openai: 'chatgpt',
  anthropic: 'claude',
  twitter: 'x',
  epic: 'epic-games',
};

/** 归一到有 bundled 图标的 canonical id；无则 null。 */
function canonical(id: string): BrandIconId | null {
  const key = ALIASES[id] ?? id;
  return key in BRAND_SVGS ? (key as BrandIconId) : null;
}

/**
 * 解析内置品牌图标。命中 → 内联 SVG 节点；未命中（自定义/其它应用）→ null。
 * 调用方用返回值做「有内置图标就用本地 SVG，否则回退」的分流。
 */
export function brandIcon(id: string): ReactNode | null {
  const key = canonical(id);
  if (!key) return null;
  // 用 <img data:svg> 而非内联 SVG：品牌图标 viewBox 千差万别（方/tall/带偏移），`object-fit` 对
  // **内联 svg 在 WebKit 不可靠**；渲染成 <img> 后 contain 对 replaced 元素稳定生效。
  // 图标不裁切还依赖承载 tile 不是 grid 容器，见 index.css「图标 tile 不用 grid 承载」。
  const src = `data:image/svg+xml,${encodeURIComponent(BRAND_SVGS[key])}`;
  return <img className="brand-svg" src={src} alt="" aria-hidden draggable={false} />;
}

/** 是否存在内置品牌图标（供调用方在不需要节点时判定分流）。 */
export function hasBrandIcon(id: string): boolean {
  return canonical(id) !== null;
}
