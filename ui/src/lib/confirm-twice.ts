/**
 * 破坏性操作的「原地二次点击」确认 —— 全仓**唯一**实现，1:1 对齐原型 `confirmTwice`
 * （`~/docs/polaris/design/prototype/polaris-prototype.html` L3211-3218）。
 *
 * 原型逐条语义（本文件按条落地，改任一条都是对拍偏差）：
 *  1. 首次点击：按钮内文案换成提示语 + 加 `.confirming` 类 + `dataset.confirm='1'`（此处 = `armed === key`）；
 *  2. **2600ms** 未二次点击自动复位（复原文案 + 去 `.confirming`）—— L3217 的字面量；
 *  3. 第二次点击：**先清 timeout**、复位，再执行 action；
 *  4. 图标按钮（无 `<span>`）不换文案，只翻红（原型 `node-del` / `app-remove` 走的就是这条）。
 *
 * 为什么必须收敛成一份：同一交互类此前在本仓有三套写法 —— LogsScreen（timeout 复位）、
 * ConnectionsScreen（`onBlur` 复位、**无** timeout）、SettingsAbout/SettingsHelper/NodesScreen
 * （自绘 `ConfirmDialog` 弹窗）。用户在不同屏得到不同的肌肉记忆，这本身就是缺陷。
 *
 * 分成「纯工厂 + 极薄 hook」而不是直接写 hook：本仓 vitest 是 `environment:'node'`（无 jsdom、
 * 无 testing-library，有意为之），hook 渲染不了 ⇒ 超时/状态机若只活在 hook 里就**测不到**。
 * `createConfirmTwice` 零 React 依赖，可在 node 下用假时钟直测（见 `confirm-twice.test.ts`）。
 */

import { useEffect, useRef, useState } from 'react';

/**
 * 未二次点击时自动复位的时长 —— 逐字取自原型 L3217 的 `2600`。
 * 全仓只此一处：任何屏里再出现一个自己写的 2.6s 常量，都会被 `destructive-confirm-wiring.test.ts` 判红。
 */
export const CONFIRM_TWICE_MS = 2600;

export interface ConfirmTwiceCore {
  /**
   * 点击入口。首次（或换了别的 key）→ 武装并起自动复位定时器；同一 key 再点 → 清定时器、复位、执行 `action`。
   *
   * `action` 同步调用，异步腿由调用点自己 `void asyncFn()` —— 与原型 `action()` 同形，
   * 本文件不接管错误处理（各屏的失败提示语境不同，收进来只会变成又一层要绕开的抽象）。
   */
  confirmTwice: (key: string, action: () => void) => void;
  /**
   * 外部撤销武装：清定时器 + 清 armed + 经 `onChange` 通知。幂等（未武装时 no-op，不空发通知）。
   *
   * 用于「用户把注意力移开了」——点了页面别处、点了别的按钮。原型只有超时一条复位路径，
   * 于是一颗翻红的按钮会在用户已经走神之后继续挂 2.6s，此时的任何一次点击都直接执行破坏性动作。
   */
  reset: () => void;
  /** 只清定时器（不改 armed）。组件卸载时调 —— 否则 2.6s 后会在已卸载组件上 setState。 */
  dispose: () => void;
}

/**
 * 纯逻辑核心（零 React）。`onChange` 是唯一出口：武装/复位都经它通知外界。
 *
 * **单槽**（同时只有一个 key 处于待确认态）：原型的状态挂在各按钮自己的 `dataset` 上，理论上可同时
 * 武装多颗；这里换成单槽 = 武装 B 会顺手解除 A。这是**有意收紧**（同一屏上同时挂着两个「再点一次就删」
 * 的按钮，误触面更大），且方向安全 —— 收紧后只会少删，不会多删。已在交回里点名。
 */
export function createConfirmTwice(
  onChange: (armed: string | null) => void,
  timeoutMs: number = CONFIRM_TWICE_MS,
): ConfirmTwiceCore {
  let armed: string | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const clear = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };

  return {
    confirmTwice(key, action) {
      // 原型 L3213：`if(btn.dataset.confirm==='1'){ clearTimeout(...); ...; action(); return; }`
      // 清 timeout **在** action 之前：action 可能同步卸载本组件（删掉最后一个节点即如此），
      // 留着的定时器就会打到已卸载的组件上。
      if (armed === key) {
        clear();
        armed = null;
        onChange(null);
        action();
        return;
      }
      clear();
      armed = key;
      onChange(key);
      timer = setTimeout(() => {
        timer = null;
        armed = null;
        onChange(null);
      }, timeoutMs);
    },
    reset() {
      if (armed === null) return; // 幂等：未武装时不空发 onChange（否则每次点击都触发一次重渲）
      clear();
      armed = null;
      onChange(null);
    },
    dispose: clear,
  };
}

export interface ConfirmTwice {
  /** 当前处于「再点一次即执行」待定态的 key；`null` = 无。渲染侧据此加 `.confirming` 与提示文案。 */
  armed: string | null;
  /** 见 [`ConfirmTwiceCore.confirmTwice`]。引用恒稳定，可直接进 `useCallback` 依赖数组。 */
  confirmTwice: ConfirmTwiceCore['confirmTwice'];
}

/**
 * 武装态元素统一带的类名（全仓 12 个二次确认站点一致，含整卡触发面 `.nd-card.confirming`）。
 * 下面的「点别处即复原」用它判断一次点击是否落在**当前触发面**内。
 */
const CONFIRMING_CLASS = '.confirming';

/** React 侧的薄封装：state 存 armed，卸载时 `dispose()` 清定时器。 */
export function useConfirmTwice(): ConfirmTwice {
  const [armed, setArmed] = useState<string | null>(null);
  const coreRef = useRef<ConfirmTwiceCore | null>(null);
  coreRef.current ??= createConfirmTwice(setArmed);
  // 卸载清定时器：原型是裸 DOM，按钮随节点一起消失、定时器打在孤儿元素上无害；
  // React 下不清就是在已卸载组件上 setState。
  useEffect(() => () => coreRef.current?.dispose(), []);

  /**
   * 点击落在武装元素之外 → 立刻复原（陈先生 2026-07-30）。此前唯一的复位路径是 2.6s 超时，
   * 于是用户点开别处、注意力已经移走，那颗按钮仍挂着「再点一次就删」——回头随手一点即执行。
   *
   * 判据用 `.confirming` 而不是记 ref：触发面有整卡（`.nd-card`）也有图标按钮，形态不一，
   * 而这个类是全仓统一加的、正好标出「当前武装的那块可点区域」。落在它内部 → 那正是第二次点击，
   * 放行给按钮自己的 onClick；落在外面 → 复原。
   *
   * **capture 阶段**：要抢在目标元素自己的 click 之前判定。用 `pointerdown` 而非 `click`——
   * 按下即表意，且 click 在跨元素拖拽时可能根本不触发。
   */
  useEffect(() => {
    if (armed === null) return;
    const onPointerDown = (e: PointerEvent) => {
      const el = e.target as Element | null;
      if (el?.closest?.(CONFIRMING_CLASS)) return;
      coreRef.current?.reset();
    };
    document.addEventListener('pointerdown', onPointerDown, true);
    return () => document.removeEventListener('pointerdown', onPointerDown, true);
  }, [armed]);

  return { armed, confirmTwice: coreRef.current.confirmTwice };
}
