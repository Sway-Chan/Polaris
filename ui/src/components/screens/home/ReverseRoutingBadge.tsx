/**
 * 反向（回国）分流持久指示器 —— 挂在主页「分流策略」标签行，紧邻 smart/global/direct seg2。
 *
 * 背景（真机 2026-07-20 §1.4「零可见性」）：`regionRouting.reverse` 唯一入口是规则页地区卡右下角的
 * 小胶囊按钮，主页/状态栏/侧栏/托盘全无呈现，唯一语义提示是切换瞬间的一次性 toast。用户误触后主页
 * 仍只显示「智能 · 已连接」，看不出分流语义已反转（本地走代理、海外直连）——叠加规则集缺失后退化成
 * 全量明文直连且零告警。
 *
 * 设计：复用既有 `.pill.warn`（components.css:65），不引入新视觉；`reverse=false` 直接返回 null，
 * 正向语义下零额外 DOM、零噪音。**判据在 `domain/region-routing.ts:isReverseRegionRouting`**，
 * 本组件只负责呈现，故 props 收 `reverse` 布尔而非整个 config（便于无 DOM 环境下渲染断言）。
 */

import { useTranslation } from 'react-i18next';

export interface ReverseRoutingBadgeProps {
  /** 见 domain/region-routing.ts `isReverseRegionRouting(config)`。 */
  reverse: boolean;
}

export function ReverseRoutingBadge({ reverse }: ReverseRoutingBadgeProps) {
  const { t } = useTranslation();
  if (!reverse) return null;
  return (
    <span
      className="pill warn"
      style={{ marginLeft: 6 }}
      data-tip={t('home.reverseRoutingTip')}
    >
      {t('home.reverseRoutingBadge')}
    </span>
  );
}

export default ReverseRoutingBadge;
