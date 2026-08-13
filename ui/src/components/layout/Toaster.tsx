/**
 * Toaster —— 全局 toast 宿主（原型 `notify()` L3199-3209 的 React 移植）。
 *
 * # 为什么必须有
 *
 * `lib/error-handler.ts` 的 `toast` 门面一直是 **console 桩**（`setToastImpl` 全仓零调用），而全库 20+
 * 处用户反馈以它为**唯一**通道（启停失败、测速缺席报数、FakeIP 自动纠正、切节点重启预判…）——
 * 于是那些「已修好的反馈」全落进 console，用户一个都看不见。这是与本轮审计所修的「UI 看着能用、
 * 后端其实没接」同款的假接线，只是方向相反：接线做了，出口没接。
 *
 * # 对齐原型的行为（勿凭喜好改）
 *
 * - 位置/层级：`#toast-stack` 右下、`pointer-events:none` 不挡操作。**挂载点随弹窗栈走**：无弹窗时
 *   挂 `.win`（原型 `notify()` 的形态）；**有弹窗时 portal 进最顶层 `<dialog>` 的子树** —— top-layer
 *   元素及其 `::backdrop` 恒压普通流且 z-index 无效，不进 dialog 子树的 toast 在弹窗打开时必被遮住。
 *   根因、双引擎实测与「为什么不是 Popover API」见 `../dialogs/dialog-top-layer.ts` 模块头。
 *   随之 `position` 必须是 **fixed 而非 absolute**：dialog 子树内的 absolute 会相对那 460px 的弹窗
 *   定位（实测落到弹窗内部右下角），fixed 才仍相对视口 —— top-layer 不改变 fixed 的包含块。
 *   **bottom 不在这里**：它必须跟着状态栏高度走，故由 index.css 的
 *   `#toast-stack{ bottom:calc(var(--statusbar-h) + var(--toast-gap)) }` 给出（推导与原型 96 为何是死账，
 *   见该处注释）。此处只留与任何元素尺寸无关的布局不变量（right/z-index/栈方向）；
 *   若在下方内联 style 里重新写 bottom，会以内联优先级压掉那条推导 —— 有单测守卫（styles/style-invariants.test.ts）。
 * - **栈上限 2**：原型 `while(c.children.length>=2) c.firstChild.remove()` —— 超出即挤掉最旧的，
 *   不是无限堆叠（连续失败时屏幕不会被糊满）；
 * - 2200ms 后淡出、200ms 后卸载；`.show` 类驱动进场（CSS 已在 prototype.css:578-587，含 dark 收敛）；
 * - kind → class：ok/err 有专属配色，info/warning 走基础样式（原型只有三档，不自造第四档）。
 *
 * # 原型之外的两样能力（`key` / `sticky`）
 *
 * 原型的 `notify()` 只服务「一次性通知」。**持续状态**（测速进度）需要另外两样，缺一样就退化成刷屏：
 *  - `key`：同 key 的后续调用更新那一条，不新增（否则 50 个节点 = 50 条 toast）；
 *  - `sticky`：不自动消失（否则一轮测速十几秒，2.2s 的 toast 只够显示前两个节点）。
 * 两者只在带 `ToastOptions` 调用时生效，**不带就是原型行为逐字不变**（既有 20+ 处调用点零改动）。
 *
 * # 第三样：`action`（行内按钮）
 *
 * 唯一消费者是测速**中断态**的「继续」（续测差集，见 `lib/speedtest-progress-toast.ts`）。中断这件事
 * 用户此刻多半正在做别的（他刚点了断开/切节点），所以既不能自动续（抢后端单飞闸 + 反直觉），也不能
 * 让他去别处找入口 —— 按钮就长在报告这件事的那条 toast 上是最短路径。
 *
 * 两条形态约束由 `toast-queue.ts` 钉死，不在本组件复刻：① 带 action 的 toast **一定有出路**
 * （`autoDismissMs` 让 action 压过 sticky，返回有限停留）；② 停留时长明显长于 2.2s（否则按钮点不到）。
 * 本组件只负责渲染按钮 + 把该条的 `pointer-events` 收回来（栈整体是 none，不然按钮点不着）。
 * 样式 `.toast-action` 落在 `styles/index.css`（`prototype.css`/`components.css` 是冻结原型，禁改）。
 * 判定全在 `toast-queue.ts`（纯逻辑，可单测——本组件在 node 环境的 vitest 里测不了）；
 * 「持续状态该 sticky」与 `SettingsTun.tsx:164`「一次性通知不该用 toast 承载常驻风险」为何不冲突，
 * 见 `toast-queue.ts` 文件头第三节。
 *
 * 顺带白捡的一条：toast 已 portal 进 top-layer（见上），故进度 toast **弹窗打开时照样可见**——
 * 测速期间用户去开节点编辑弹窗，进度不会被 `::backdrop` 吃掉。
 */

import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { setToastImpl, type ToastOptions } from '@/lib/error-handler';
import { useTopDialogEl } from '../dialogs/dialog-top-layer';
import {
  autoDismissMs,
  toastListKey,
  upsertToast,
  type ToastEntry,
  type ToastKind,
} from './toast-queue';

const LEAVE_MS = 200;

export function Toaster() {
  const [items, setItems] = useState<ToastEntry[]>([]);
  // 最顶层打开中的 <dialog>（无弹窗 = null）。订阅式：弹窗中途关闭时挂载点要迁回 `.win`，
  // 否则 portal 容器被移除、正在播的 toast 当场消失（提交成功即关窗那条路径必踩）。
  const topDialog = useTopDialogEl();
  const seq = useRef(0);
  // 卸载时清空所有在飞定时器（严格模式双挂载 / 热重载下防泄漏）。
  const timers = useRef<Set<ReturnType<typeof setTimeout>>>(new Set());

  useEffect(() => {
    const later = (fn: () => void, ms: number) => {
      const id = setTimeout(() => {
        timers.current.delete(id);
        fn();
      }, ms);
      timers.current.add(id);
      return id;
    };

    const push = (msg: string, kind: ToastKind, desc?: string, opts?: ToastOptions) => {
      const id = ++seq.current;
      const entry: ToastEntry = {
        id,
        dedupeKey: opts?.key,
        msg,
        desc: (desc ?? opts?.description)?.trim() || undefined,
        kind,
        sticky: !!opts?.sticky,
        action: opts?.action,
        shown: false,
        leaving: false,
      };
      // 队列语义（同 key 原地更新 / 溢出优先挤非 sticky）全在纯逻辑里，本组件不复刻一份。
      setItems((prev) => upsertToast(prev, entry));
      // 下一帧加 .show（与原型的 requestAnimationFrame 同义）：同帧加类不会触发 transition。
      // 同 key 更新时 `upsertToast` 已沿用旧 `shown`，这一发是无害的重复置位（不会重播动画）。
      requestAnimationFrame(() =>
        setItems((prev) => prev.map((it) => (it.id === id ? { ...it, shown: true } : it)))
      );
      // `null` = sticky，不起淡出定时器。判定在 toast-queue（组件在 node 环境测不了，策略必须可单测）。
      const ttl = autoDismissMs(entry);
      if (ttl === null) return;
      later(() => {
        setItems((prev) => prev.map((it) => (it.id === id ? { ...it, leaving: true } : it)));
        later(() => setItems((prev) => prev.filter((it) => it.id !== id)), LEAVE_MS);
      }, ttl);
    };

    // 注入真实实现，替换 console 桩。门面已被全库消费，故这里只需接上出口。
    setToastImpl({
      success: (m, o) => push(m, 'ok', undefined, o),
      // 第二参 description（错误原因）此前被丢；接上出口，两段展示（见 ToastEntry.desc / 渲染 .toast-desc）。
      error: (m, d, o) => push(m, 'err', d, o),
      info: (m, o) => push(m, '', undefined, o),
      warning: (m, o) => push(m, '', undefined, o),
    });

    const pending = timers.current;
    return () => {
      pending.forEach(clearTimeout);
      pending.clear();
    };
  }, []);

  if (items.length === 0) return null;

  const stack = (
    <div
      id="toast-stack"
      style={{
        // fixed（不是 absolute）：见文件头 —— portal 进 dialog 子树后 absolute 会相对弹窗定位。
        // 无弹窗时挂 `.win`，而 `.win` 恰好铺满视口（index.css:93 `width:100%;height:100vh`），
        // 故两种挂载点下 fixed 落点一致，不需要按挂载点切换定位方式。
        position: 'fixed',
        right: 24,
        // bottom 刻意缺席：见文件头注释 —— 由 index.css `#toast-stack` 从 --statusbar-h 推导。
        zIndex: 400,
        display: 'flex',
        flexDirection: 'column',
        gap: 8,
        alignItems: 'flex-end',
        pointerEvents: 'none',
      }}
    >
      {items.map((it) => (
        <div
          // 不是 `it.id`：同 key 更新会换新 id，用 id 作 React key 等于每次刷新都卸载重挂
          // ⇒ 进场动画重播，进度 toast 每收一个事件闪一次。见 toast-queue.ts `toastListKey`。
          key={toastListKey(it)}
          className={`toast${it.kind ? ` ${it.kind}` : ''}${it.shown && !it.leaving ? ' show' : ''}`}
          role="status"
          aria-live="polite"
          // 栈整体是 `pointer-events:none`（不挡操作，原型语义）。带按钮的这一条必须把它收回来，
          // 否则按钮点不到 —— 且只收这一条，无按钮的 toast 仍旧完全不挡鼠标。
          style={it.action ? { pointerEvents: 'auto' } : undefined}
        >
          <div className="toast-msg">{it.msg}</div>
          {it.desc && <div className="toast-desc">{it.desc}</div>}
          {it.action && (
            <button type="button" className="toast-action" onClick={it.action.onClick}>
              {it.action.label}
            </button>
          )}
        </div>
      ))}
    </div>
  );

  return topDialog ? createPortal(stack, topDialog) : stack;
}

export default Toaster;
