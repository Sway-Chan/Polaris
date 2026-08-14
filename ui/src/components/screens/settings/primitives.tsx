/**
 * settings 屏共享原语 —— 逐字对齐原型 CSS 类，不叠加 Tailwind 工具类重新发明样式。
 *
 * 原型样式真值源：polaris-prototype.html <style>（已 1:1 移植进 prototype.css，勿改）。
 * 这里只负责拼出与原型完全相同的 class 名 + DOM 结构；尺寸/颜色/间距全部交给 prototype.css，
 * 不在 className 里堆 Tailwind utility 重新发明一遍（那正是 A1 台账记录的漂移根因）。
 *
 * 九个子页统一以这些原语拼装，保证视觉与交互一致。
 */

import type {
  ReactNode,
  ReactElement,
  SelectHTMLAttributes,
  InputHTMLAttributes,
  ButtonHTMLAttributes,
  CSSProperties,
  ChangeEvent,
} from 'react';
import { Children, isValidElement } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Csel, type CselOption } from '../../dialogs/Csel';

/* ── phead：页头（h1 + sub 描述 + 右侧 acts） ── 原型 .phead L255 */
export function Phead({
  title,
  sub,
  children,
}: {
  title: ReactNode;
  sub?: ReactNode;
  children?: ReactNode;
}) {
  return (
    <div className="phead">
      <div>
        <h1>{title}</h1>
        {sub && <div className="sub">{sub}</div>}
      </div>
      {children && <div className="acts">{children}</div>}
    </div>
  );
}

/* ── card：通用卡片 ── 原型 .card L269 */
export function Card({
  children,
  className,
  pad,
  id,
  style,
}: {
  children: ReactNode;
  className?: string;
  /** .card.pad = padding:16px */
  pad?: boolean;
  id?: string;
  style?: CSSProperties;
}) {
  return (
    <div id={id} className={cn('card', pad && 'pad', className)} style={style}>
      {children}
    </div>
  );
}

/* ── set-block：设置分组卡（.set-block-h 头 + 若干 .set-row） ── 原型 .set-block L1123 */
export function SetBlock({
  header,
  children,
  className,
  id,
}: {
  header?: ReactNode;
  children: ReactNode;
  className?: string;
  id?: string;
}) {
  return (
    <div id={id} className={cn('card', 'set-block', className)}>
      {header && <div className="set-block-h">{header}</div>}
      {children}
    </div>
  );
}

/**
 * 一个设置项及其从属内容（如「开关 + 可展开清单」）。组内不画分隔线，分隔线只落在完整设置项之间，
 * 避免把清单误画成与其开关并列的另一项。不要用它包多个彼此独立的 SetRow。
 */
export function SetRowGroup({ children }: { children: ReactNode }) {
  return <div className="set-row-group">{children}</div>;
}

/** 设置卡片中位于自定义内容之后的一组标准行；统一承担组前分隔与末行收口。 */
export function SetRowSection({ children }: { children: ReactNode }) {
  return <div className="set-row-section">{children}</div>;
}

/* ── card-h / card-sub：卡内标题 + 副说明 ── 原型 .card-h L271 / .card-sub L272 */
export function CardH({ children }: { children: ReactNode }) {
  return <div className="card-h">{children}</div>;
}
export function CardSub({
  children,
  className,
  id,
  style,
}: {
  children: ReactNode;
  className?: string;
  id?: string;
  style?: CSSProperties;
}) {
  return (
    <div id={id} className={cn('card-sub', className)} style={style}>
      {children}
    </div>
  );
}

/* ── set-row：单行设置项（左文本 + 右控件） ── 原型 .set-row L1129
 *
 * .sr-ctl 在原型里直接挂在控件元素本身（`<span class="sr-ctl swt">` / `<div class="sr-ctl" style="…">`），
 * 不是控件外再套一层。这里的 wrapper div 就是那唯一一层 —— ctrlClassName/ctrlStyle 供个别行需要
 * 把 sr-ctl 直接当 flex 容器用（如管理面板 secret 行、DNS 双列 select+input 堆叠）。
 * children 为 undefined 时不渲染 sr-ctl（原型里纯文案行——如列表编辑器上方的说明行——没有右侧控件）。 */
export function SetRow({
  label,
  desc,
  children,
  className,
  style,
  align,
  ctrlClassName,
  ctrlStyle,
  id,
}: {
  label: ReactNode;
  desc?: ReactNode;
  children?: ReactNode;
  className?: string;
  style?: CSSProperties;
  /** 快捷方式：align-items:flex-start（原型多处 `style="align-items:flex-start"`） */
  align?: 'start';
  ctrlClassName?: string;
  ctrlStyle?: CSSProperties;
  id?: string;
}) {
  const rowStyle = style ?? (align === 'start' ? { alignItems: 'flex-start' as const } : undefined);
  return (
    <div id={id} className={cn('set-row', className)} style={rowStyle}>
      <div className="sr-tx">
        <b>{label}</b>
        {desc && <div>{desc}</div>}
      </div>
      {children !== undefined && (
        <div className={cn('sr-ctl', ctrlClassName)} style={ctrlStyle}>
          {children}
        </div>
      )}
    </div>
  );
}

/* ── Switch：开关（.swt） ── 原型 .swt L297
 * 空元素，knob 由 CSS ::after 画（非我们手写的内层 span）——逐字对齐原型的 `<span class="swt">`。
 * 原型用 <span data-act> 靠事件委托；这里换成可键盘操作的 role=switch，不改视觉 DOM。 */
export function Switch({
  checked,
  onChange,
  disabled,
  /** 半选态（原型 .swt.indet L303，如备份「全选」主开关部分勾选） */
  indeterminate,
  className,
  id,
  /** disabled=true 时的悬浮提示——「假可用」标记统一走 disabled + tip（见 NodesScreen 移动到分组按钮范式）。 */
  tip,
  'aria-label': ariaLabel,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  disabled?: boolean;
  indeterminate?: boolean;
  className?: string;
  id?: string;
  /** 走统一 tooltip 引擎（`data-tip`），不是原生 `title`——理由见 lib/tooltip-engine.ts 头注。 */
  tip?: string;
  'aria-label'?: string;
}) {
  return (
    <span
      id={id}
      role="switch"
      tabIndex={disabled ? -1 : 0}
      aria-checked={indeterminate ? 'mixed' : checked}
      aria-label={ariaLabel}
      aria-disabled={disabled || undefined}
      data-tip={tip}
      onClick={() => !disabled && onChange(!checked)}
      onKeyDown={(e) => {
        if (disabled) return;
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onChange(!checked);
        }
      }}
      className={cn('swt', indeterminate ? 'indet' : checked && 'on', className)}
      /* 只降不透明度、不设 pointer-events:none —— 后者会把该 span 移出命中测试，
       * disabled 态搭配的 tip 提示（「假可用」标注的原因说明）就永远碰不到 hover，
       * 等于标了原因也没人看得到。原生点击守卫已在 onClick/onKeyDown 里判 disabled，
       * 不靠 pointer-events 挡误触发。（真机 WebKitGTK hit-test 探针已验证：见交付说明） */
      style={disabled ? { opacity: 0.5 } : undefined}
    />
  );
}

/* ── Segmented：分段控件（.seg2） ── 原型 .seg2 L304 */
export interface SegOption<T extends string> {
  value: T;
  label: ReactNode;
}

export function Segmented<T extends string>({
  options,
  value,
  onChange,
  ariaLabel,
  id,
  className,
  disabled,
  /** disabled=true 时的悬浮提示——同 Switch/Select 的「假可用」disabled + tip 范式。 */
  tip,
}: {
  options: readonly SegOption<T>[];
  value: T;
  onChange: (next: T) => void;
  ariaLabel?: string;
  id?: string;
  className?: string;
  disabled?: boolean;
  /** 走统一 tooltip 引擎（`data-tip`），不是原生 `title`。 */
  tip?: string;
}) {
  return (
    <div
      id={id}
      role="radiogroup"
      aria-label={ariaLabel}
      aria-disabled={disabled || undefined}
      data-tip={tip}
      className={cn('seg2', className)}
      /* 同 Switch：不设 pointer-events:none，否则容器 hover 连 tip 都碰不到（理由见 Switch 注释）。 */
      style={disabled ? { opacity: 0.5 } : undefined}
    >
      {options.map((opt) => {
        const on = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={on}
            disabled={disabled}
            onClick={() => !disabled && onChange(opt.value)}
            className={cn(on && 'on')}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

/* ── Select：下拉 ── 统一复用 `.csel` 自定义下拉。
 * 原生 <select> 在 macOS/Windows 的**展开 <option> 弹层无法样式化**，恒为系统默认 UI（真机实测：
 * 与深色玻璃拟态界面割裂）。原型对此提供 `.csel`（div 触发器 + 可样式化 `.csel-menu`），本应用 dialog
 * 层已有 <Csel> 组件（受控、fixed 定位、脱离 Modal 也可用——ctx 已 guard）。此处把 Select 内部实现
 * Settings 子页全部经本组件复用 <Csel>，弹层不会露出系统默认元素。
 * 消费方 API 不变（仍传 value/onChange(event)/<option> children）：从 <option> children 提取选项，
 * onChange 合成最小事件（e.target.value），既有 `e.target.value` 消费点零改动。 */
export function Select({
  className,
  style,
  children,
  value,
  onChange,
  id,
  disabled,
  /** disabled=true 时的悬浮提示——之前这里没接住提示文案，调用方传了也会被静默丢弃（漏接线）。 */
  tip,
  'aria-label': ariaLabel,
}: SelectHTMLAttributes<HTMLSelectElement> & {
  className?: string;
  style?: CSSProperties;
  /** 走统一 tooltip 引擎（`data-tip`），不是原生 `title`。 */
  tip?: string;
}) {
  const options: CselOption[] = Children.toArray(children)
    .filter((c): c is ReactElement<{ value?: string | number; children?: ReactNode; disabled?: boolean }> =>
      isValidElement(c) && c.type === 'option',
    )
    .map((opt) => ({
      value: String(opt.props.value ?? ''),
      label: typeof opt.props.children === 'string' ? opt.props.children : String(opt.props.children ?? ''),
      disabled: opt.props.disabled,
    }));

  return (
    <div
      className={cn(className)}
      /* 不设 pointer-events:none，否则 wrapper hover 连 tip 都碰不到（理由见 Switch 注释）。 */
      style={disabled ? { ...style, opacity: 0.5 } : style}
      aria-disabled={disabled || undefined}
      data-tip={tip}
    >
      <Csel
        value={String(value ?? '')}
        disabled={disabled}
        onChange={(next) => {
          onChange?.({
            target: { value: next },
            currentTarget: { value: next },
          } as unknown as ChangeEvent<HTMLSelectElement>);
        }}
        options={options}
        id={id}
        ariaLabel={ariaLabel}
      />
    </div>
  );
}

/* ── TextInput：输入框（.input） ── 原型 .input L311 */
export function TextInput({
  className,
  ...rest
}: InputHTMLAttributes<HTMLInputElement> & { className?: string }) {
  return <input className={cn('input', className)} {...rest} />;
}

/* ── Button：按钮（.btn） ── 原型 .btn L273 */
export function Button({
  variant = 'default',
  size = 'md',
  className,
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'default' | 'ghost' | 'flow';
  size?: 'md' | 'sm';
  children: ReactNode;
  /**
   * 悬浮提示，走统一 tooltip 引擎（见 lib/tooltip-engine.ts）。显式声明而非靠 JSX 对连字符属性
   * 免检放行——下面的 `hoverableDisabled` 要**读**它，读就得有类型。
   */
  'data-tip'?: string;
}) {
  // .btn:disabled{pointer-events:none}（components.css/prototype.css）把禁用按钮整个移出命中测试——
  // disabled+提示（「假可用」标注惯用法）的浮层因此永远 hover 不到（真机 WebKitGTK hit-test 探针
  // 已验证：加这行前 elementFromPoint 命中穿透到父级，加后命中按钮自身）。只在「disabled 且带提示」
  // 时用内联 style 覆盖 pointer-events（内联样式优先级天然高于外部样式表，不用碰 CSS 文件）——
  // 不对纯 busy 态（disabled 无提示，如请求进行中）的按钮生效，避免其意外获得 hover 高亮。
  // 覆盖后仍不会误触发：原生 disabled 属性本身就挡真实点击，不依赖 pointer-events。
  // 迁到 data-tip 后这条**更要紧**：引擎靠 mouseover 委托，命中测试拿不到就连事件都收不到。
  const hoverableDisabled = rest.disabled && rest['data-tip'];
  return (
    <button
      type="button"
      {...rest}
      style={hoverableDisabled ? { ...rest.style, pointerEvents: 'auto' } : rest.style}
      className={cn(
        'btn',
        variant === 'ghost' && 'ghost',
        variant === 'flow' && 'flow',
        size === 'sm' && 'sm',
        className,
      )}
    >
      {children}
    </button>
  );
}

/* ── Pill：标签胶囊（.pill） ── 原型 .pill L285 */
export function Pill({
  variant = 'default',
  className,
  style,
  children,
}: {
  variant?: 'default' | 'ok' | 'warn' | 'err' | 'region' | 'proto';
  className?: string;
  style?: CSSProperties;
  children: ReactNode;
}) {
  return (
    <span className={cn('pill', variant !== 'default' && variant, className)} style={style}>
      {children}
    </span>
  );
}

/* ── Dot：状态圆点（.dot） ── 原型 .dot L293
 * variant='flow' 无预定义 modifier class（原型该态用内联 style 覆写背景色），走 style 透传。 */
export function Dot({
  variant = 'idle',
  style,
}: {
  variant?: 'ok' | 'err' | 'idle' | 'flow';
  style?: CSSProperties;
}) {
  return <span className={cn('dot', variant !== 'flow' && variant)} style={style} />;
}

/* ── ProgressBar：进度条（.bar > i） ── 原型 .bar L325 */
export function ProgressBar({ value, style }: { value: number; style?: CSSProperties }) {
  return (
    <div className="bar" style={style}>
      <i style={{ width: `${Math.max(0, Math.min(100, value))}%` }} />
    </div>
  );
}

/* ── Spinner：加载圈（.spinner） ── 原型 .spinner */
export function Spinner({ className }: { className?: string }) {
  const { t } = useTranslation();
  return <span className={cn('spinner', className)} role="status" aria-label={t('common.loading')} />;
}

/* ── InfoIcon：圆形信息提示（.info-i） ── 原型 .info-i L1028 */
export function InfoIcon({ tip }: { tip: string }) {
  return (
    // `aria-label` 与 `data-tip` 同文案：前者是这个图标的**无障碍名**（它没有可见文字），
    // 后者是视觉浮层。不是两套打架——引擎的 `aria-describedby` 是补充说明，替代不了名字。
    <span className="info-i" tabIndex={0} role="img" aria-label={tip} data-tip={tip}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
        <circle cx="12" cy="12" r="9" />
        <path d="M12 11v5M12 8h.01" />
      </svg>
    </span>
  );
}
