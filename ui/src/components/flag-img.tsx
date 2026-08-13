/**
 * `<FlagImg>` —— 旗面**渲染器**（纯展示，语义中立）。状态栏、首页「出口节点」框、首页节点选单共用。
 *
 * # 为什么是一个语义中立的渲染器，而不是每种语义一个组件
 *
 * 本仓有两种旗面**语义**，它们必须分开的是**数据源**，不是渲染体：
 *
 * | 位置 | 数据源 | 语义 |
 * |---|---|---|
 * | 状态栏 / 首页「出口节点」框 | 出口 IP 探测的 countryCode（`@/domain/exit-flag`） | 「我现在**从哪出去**」 |
 * | 首页节点选单 / 节点列表 | 名称派生（`flag-detect.getCountryCode`） | 「这个节点**自称**在哪」 |
 *
 * 两者画出来的东西逐属性完全相同（`.flag` 盒 + data URI + `aria-hidden`），故渲染只留这一份；
 * 分叉收在**调用方选哪个 code 源**上。曾经按语义各写一份（`ExitFlag` / `NmFlag`）——那是把「数据源
 * 要分开」误读成「渲染器要分开」，两份实现必然漂移。
 *
 * **无码 → 返回 `null`（什么都不画）**，不回落地球占位：地球会被读成「在某个未知国家」，
 * 而真相是「不知道」。
 *
 * `className` 由调用方给（`.flag` 定 18×12 盒，`.flag.sb` 收成状态栏的 15×10；见 styles/prototype.css）。
 * `data-flag-code` 供测试/工具按国家码断言，不影响渲染（同 `nodes/nd-flag.tsx` 范式）。
 *
 * 卡片右下角的 `NdFlag`（`nodes/nd-flag.tsx`）**不并进来**：它用 `.nd-flag` 类、是绝对定位的大水印，
 * 与本组件的行内 `.flag` 盒不是同一个渲染体。
 */
import type { ReactNode } from 'react';
import { countryCodeToFlagAsset } from '@/domain/flag-assets';
import { cn } from '@/lib/utils';

export function FlagImg({
  code,
  className,
}: {
  /** 地区码；`null` = 未探到 / 未识别 → 不渲染。 */
  code: string | null;
  className?: string;
}): ReactNode {
  const asset = code ? countryCodeToFlagAsset(code) : null;
  if (!asset) return null;
  return (
    <img
      className={cn('flag', className)}
      src={asset.src}
      alt=""
      data-tip={asset.label}
      aria-hidden="true"
      draggable={false}
      data-flag-code={asset.code}
    />
  );
}

export default FlagImg;
