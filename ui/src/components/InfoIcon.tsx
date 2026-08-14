/**
 * 全局信息提示原语。复杂说明统一收进 tooltip，避免每个页面各画一套 `i` 图标、
 * 各自遗漏键盘焦点或可访问名称。
 */
export function InfoIcon({ tip, className = '' }: { tip: string; className?: string }) {
  return (
    <span
      className={`info-i${className ? ` ${className}` : ''}`}
      role="img"
      tabIndex={0}
      aria-label={tip}
      data-tip={tip}
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
        <circle cx="12" cy="12" r="9" />
        <path d="M12 11v5M12 8h.01" />
      </svg>
    </span>
  );
}

export default InfoIcon;
