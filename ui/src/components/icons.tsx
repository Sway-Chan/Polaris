/**
 * 内嵌图标（1:1 提取自原型 polaris-prototype.html nav-item / statusbar / winctl SVG path）。
 *
 * 原型把每个图标的 <svg> 直接写在 markup 里（L1588-1598 导航 / L2433-2438 状态栏 / L1572-1574 窗口控制）。
 * 这里抽成组件方便复用；viewBox/path 与原型逐字对齐，仅改用 currentColor + 统一 props。
 * 默认尺寸由父级 className 控制（原型 nav-item 17px / statusbar 13px / winctl 12px）。
 */

import type { SVGProps } from 'react';

type IconProps = SVGProps<SVGSVGElement>;

/* ── 导航项（原型 .nav-item svg，L1588-1598）── */

export const NavHomeIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M3 11l9-8 9 8" />
    <path d="M5 10v10h14V10" />
  </svg>
);

export const NavNodesIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <rect x="3" y="4" width="18" height="7" rx="1.5" />
    <rect x="3" y="13" width="18" height="7" rx="1.5" />
    <path d="M7 7.5h.01M7 16.5h.01" />
  </svg>
);

export const NavRulesIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M4 6h16M7 12h10M10 18h4" />
  </svg>
);

export const NavAppPolicyIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M6 3v6a3 3 0 003 3h6" />
    <path d="M18 21v-6a3 3 0 00-3-3H9" />
    <circle cx="6" cy="3" r="1.6" />
    <circle cx="18" cy="21" r="1.6" />
  </svg>
);

export const NavResourcesIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M3 7l2-3h5l2 3h7a1 1 0 011 1v11a1 1 0 01-1 1H3a1 1 0 01-1-1V8a1 1 0 011-1z" />
    <path d="M12 11v6M9 14l3 3 3-3" />
  </svg>
);

export const NavConnectionsIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M3 12h4l3 7 4-14 3 7h4" />
  </svg>
);

export const NavLogsIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M5 4h11l3 3v13H5z" />
    <path d="M8 9h8M8 13h8M8 17h5" />
  </svg>
);

export const NavSettingsIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <circle cx="12" cy="12" r="3.2" />
    <path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M19.1 4.9L17 7M7 17l-2.1 2.1" />
  </svg>
);

/* ── 状态栏（原型 .statusbar svg，L2433/2438）── */

export const SbNodeIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <rect x="3" y="5" width="18" height="6" rx="1" />
    <rect x="3" y="13" width="18" height="6" rx="1" />
  </svg>
);

export const SbConnsIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M3 12h4l3 7 4-14 3 7h4" />
  </svg>
);

/* `FlagGlobeIcon`（原型 `<symbol id="flag-globe">` 的 globe 兜底态）已删除：其存在前提是「真实按
   countryCode 出旗的 flag sprite 未接入」——而完整旗面系统（domain/flag-detect + flag-assets，74 区域）
   早已在仓内。用户裁定：出口地区**未探到就留空**，不画地球占位（地球会被读成「出口在某个未知国家」，
   而真相是「还不知道」）。唯一消费方 StatusBar 已改用 `components/flag-img.tsx`。 */

/* ── 品牌/折叠 chevron（原型 .brand-chev / .side-collapse，L1585/L179）── */

export const ChevronLeftIcon = (p: IconProps) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M15 6l-6 6 6 6" />
  </svg>
);

/* ── 窗口控制（原型 .winctl，L1572-1574）── */

export const WinMinIcon = (p: IconProps) => (
  <svg viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M2 6h8" />
  </svg>
);

export const WinMaxIcon = (p: IconProps) => (
  <svg viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <rect x="2.4" y="2.4" width="7.2" height="7.2" />
  </svg>
);

/** 还原（已最大化态下 WinMaxIcon 换显）：两方重叠的标准「restore」字形。 */
export const WinRestoreIcon = (p: IconProps) => (
  <svg viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <rect x="4.2" y="2.2" width="5.4" height="5.4" />
    <path d="M2.4 4v5.4h5.4" />
  </svg>
);

export const WinCloseIcon = (p: IconProps) => (
  <svg viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M3 3l6 6M9 3l-6 6" />
  </svg>
);
