/* ── Fold：受控折叠段（.fld-fold） ──
 *
 * # 位置为什么在这里，而不是 settings/primitives.tsx
 *
 * 它原本是设置页私有原语，那时注释里写着「本仓既有 5 处 `.fld-fold` 全是非受控裸 `<details>`，
 * 都在 dialog 里、生命周期短，**不在本批射程**」。2026-08-10 用户反馈「展开后要手动下拉、
 * 容易被忽视」，那 5 处（实为 6 处）进入射程 —— 若让 dialogs 反向 import
 * `screens/settings/primitives`，就是为省一次移动把层级依赖拧反。故提到 `components/` 共享层。
 *
 * # 为什么必须**受控**（`open` state + `onToggle`），不能裸 `<details>`
 *
 * 设置页每改一项就 `update()` → config 重渲，是全应用配置重渲最密集的地方。上游的同款组件
 * （`src/renderer/components/settings/conduit-controls.tsx:61-92`）注释自述踩过这个坑，故做成受控。
 * 受控的**真实作用面**：`open` 由 React state 持有，config 重渲只是重跑 render，state 不变 ⇒
 * 渲染出的 `open` 与用户上次的选择一致。裸 `<details>` 的 open 只是 DOM 自身状态，React 每次
 * 重渲都可能按 JSX 里的字面值把它写回去（`NodeDialog` 那处 `<details className="fld-fold" open>`
 * 正是这个形状）。
 *
 * # 展开即露出
 *
 * `onToggle` 除了同步 state，还调 `revealOnToggle` —— 判据与理由见 `../reveal.ts`。
 * 折叠段的价值是「默认收起、需要时展开」，而展开后看不见等于这个价值没兑现。
 *
 * # 计数徽章
 *
 * 默认折叠 ≠ 藏起来：summary 右侧恒显条目数（**0 也显示**——不显示才会让人以为功能没了）。
 * 交互形态取自**原型**的 `.ns-c` 计数徽章（CSS `proto:989`，DOM `proto:3476` / `proto:4632`）。
 * 原型那套 CSS 被 `.mini-menu / .node-menu` 前缀锁死，设置页选择器命不中 ⇒ 样式另接 `.fld-fold-c`
 * （components.css，取值逐字抄自 `.ns-c`，不另造组件族）。
 * 文案复用既有五语种键 `common.itemCount`（"{{n}} 条"），不新增键。
 *
 * `.fld-fold-t{flex:1}` 是必须的：CSS 里 `summary svg{margin-left:auto}` 在 summary 只有
 * 「标题 + 箭头」时把箭头推到右侧；插入计数后**两个 auto 会均分剩余空间**、计数飘到中间。
 * 让标题吃掉剩余空间 ⇒ svg 的 auto 无空间可分，计数与箭头自然贴右，无需覆盖既有规则。
 *
 * ⚠️ 注释里**不写各消费点的行号** —— 上一版写死的 5 个行号有 4 个已漂移、数量声明也从「四处」
 * 过期成 7 处（两轮对拍各撞一次）。注释里写别处的行号本身就是会腐烂的资产；改用「类名 + 文件名」定位。 */

import type { ReactNode } from 'react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { revealOnToggle } from './reveal';

export function Fold({
  title,
  count,
  defaultOpen,
  children,
  id,
  className,
}: {
  title: ReactNode;
  /** 条目数徽章；undefined = 不显示（如「其余平台」这类非清单折叠）。0 会照常显示。 */
  count?: number;
  defaultOpen?: boolean;
  children: ReactNode;
  id?: string;
  className?: string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(!!defaultOpen);
  return (
    <details
      id={id}
      className={cn('fld-fold', className)}
      open={open}
      onToggle={(e) => {
        setOpen(e.currentTarget.open);
        revealOnToggle(e);
      }}
    >
      <summary>
        <span className="fld-fold-t">{title}</span>
        {count !== undefined && (
          <span className="fld-fold-c">{t('common.itemCount', { n: count })}</span>
        )}
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M6 9l6 6 6-6" />
        </svg>
      </summary>
      <div className="fld-fold-body">{children}</div>
    </details>
  );
}
