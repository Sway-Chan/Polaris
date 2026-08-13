/**
 * PolarisStarSprite —— 极光之星 logo 的一次性 SVG sprite（原型 L1508-1538）。
 *
 * 隐藏的 `<svg width=0 height=0 aria-hidden>` 只承载 `#polarisStar` 的 <defs>
 * （pl-orbit/pl-dot/pl-nw…pl-wn 渐变 + pl-star 路径 + pl-clip + pl-orbitmask + <g id="polarisStar">）。
 * 所有 id 均带 `pl-` 前缀（源自原型），避免与页面其它内联 SVG id 冲突。
 *
 * 挂载一次（AppShell 顶层），各处用 `<svg viewBox="-46 -46 92 92"><use href="#polarisStar"/></svg>` 引用，
 * 尺寸/描边不由调用方决定 —— sprite 自带渐变配色，调用方不再传 currentColor/text-* 类。
 *
 * 源：~/docs/polaris/design/prototype/polaris-prototype.html L1508-1536（512 viewBox，
 * transform="translate(-50 -50) scale(0.1953125)" 映射进 -50..50 → 配合调用方 -46..46 viewBox 定位）。
 */
export default function PolarisStarSprite() {
  return (
    <svg width="0" height="0" style={{ position: 'absolute' }} aria-hidden="true">
      <defs>
        <linearGradient id="pl-orbit" x1="54" y1="72" x2="458" y2="440" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#1550F4" />
          <stop offset=".46" stopColor="#22BDEB" />
          <stop offset="1" stopColor="#1347EF" />
        </linearGradient>
        <linearGradient id="pl-dot" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#0E50F4" />
          <stop offset="1" stopColor="#149DEB" />
        </linearGradient>
        <linearGradient id="pl-nw" x1="256" y1="256" x2="174" y2="74" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#176AF5" />
          <stop offset="1" stopColor="#1BB8F0" />
        </linearGradient>
        <linearGradient id="pl-ne" x1="256" y1="256" x2="326" y2="52" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#0A4DEA" />
          <stop offset="1" stopColor="#0A36D3" />
        </linearGradient>
        <linearGradient id="pl-en" x1="256" y1="256" x2="472" y2="214" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#0E72F2" />
          <stop offset="1" stopColor="#35DCE6" />
        </linearGradient>
        <linearGradient id="pl-es" x1="256" y1="256" x2="468" y2="322" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#0C62F0" />
          <stop offset="1" stopColor="#149FEA" />
        </linearGradient>
        <linearGradient id="pl-se" x1="256" y1="256" x2="316" y2="468" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#0872EF" />
          <stop offset="1" stopColor="#22D3E3" />
        </linearGradient>
        <linearGradient id="pl-sw" x1="256" y1="256" x2="206" y2="472" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#064EE6" />
          <stop offset="1" stopColor="#083BD1" />
        </linearGradient>
        <linearGradient id="pl-ws" x1="256" y1="256" x2="42" y2="326" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#073DDC" />
          <stop offset="1" stopColor="#142CC5" />
        </linearGradient>
        <linearGradient id="pl-wn" x1="256" y1="256" x2="42" y2="208" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#0D73F0" />
          <stop offset="1" stopColor="#17C5ED" />
        </linearGradient>
        <path
          id="pl-star"
          d="M256 18 C249 91 246 139 217 184 L184 155 L202 202 C151 231 95 247 20 256 C95 265 151 281 202 310 L184 357 L217 328 C246 373 249 421 256 494 C263 421 266 373 295 328 L328 357 L310 310 C361 281 417 265 492 256 C417 247 361 231 310 202 L328 155 L295 184 C266 139 263 91 256 18 Z"
        />
        <clipPath id="pl-clip">
          <use href="#pl-star" />
        </clipPath>
        <mask id="pl-orbitmask" maskUnits="userSpaceOnUse" x="0" y="0" width="512" height="512">
          <rect width="512" height="512" fill="#fff" />
          <circle cx="369" cy="98" r="22" fill="#000" />
          <circle cx="122" cy="395" r="22" fill="#000" />
        </mask>
        <g id="polarisStar" transform="translate(-50 -50) scale(0.1953125)">
          <circle
            cx="256"
            cy="256"
            r="194"
            fill="none"
            stroke="url(#pl-orbit)"
            strokeWidth="7"
            strokeLinecap="round"
            mask="url(#pl-orbitmask)"
          />
          <circle cx="369" cy="98" r="13" fill="url(#pl-dot)" />
          <circle cx="122" cy="395" r="13" fill="url(#pl-dot)" />
          <g clipPath="url(#pl-clip)">
            <path d="M256 256 L-100 -100 L256 -100 Z" fill="url(#pl-nw)" />
            <path d="M256 256 L256 -100 L612 -100 Z" fill="url(#pl-ne)" />
            <path d="M256 256 L612 -100 L612 256 Z" fill="url(#pl-en)" />
            <path d="M256 256 L612 256 L612 612 Z" fill="url(#pl-es)" />
            <path d="M256 256 L612 612 L256 612 Z" fill="url(#pl-se)" />
            <path d="M256 256 L256 612 L-100 612 Z" fill="url(#pl-sw)" />
            <path d="M256 256 L-100 612 L-100 256 Z" fill="url(#pl-ws)" />
            <path d="M256 256 L-100 256 L-100 -100 Z" fill="url(#pl-wn)" />
          </g>
        </g>
      </defs>
    </svg>
  );
}
