/**
 * 「暂存层此刻活着没有」的**唯一** hook —— 全部 12 个编辑入口喂给 `editRoute` 的那个布尔都取自这里。
 *
 * 判定本体是纯函数 [`stagingActive`]（因果、以及为什么判据是 `coreRunning` 而不是「这条改动
 * 要不要重启」，全写在那里）。本 hook 只负责把两个 store 里的实参取齐：
 *
 *  - `enabled` —— 暂存层总开关（`staged-config-store`）；
 *  - `coreRunning` —— 内核活态（`app-store.proxyStatus`，与 `PendingChangesBar` 的 `coreRunning`
 *    同源同字段，避免条上显示的运行态与路由判定用的运行态分叉）；
 *  - `entries.length` —— 已暂存条目数。
 *
 * # 为什么单独一个文件
 *
 * 它要同时读两个 store。放进 `staged-config-store.ts` 会让它 `import` `app-store`，而 `app-store`
 * **已经** import 了 `staged-config-store`（`hydrateStagedConfig`）⇒ 循环依赖。放进
 * `lib/staged-config.ts` 会让那个纯逻辑模块（被一堆纯单测直接 import）拖进 zustand 与两个 store。
 * 故单独落在 store 层：它 import 两个 store，两个 store 都不 import 它，无环。
 *
 * # 别把它当总开关用
 *
 * 它只回答「这次编辑该走暂存还是直落盘」。`stage`/`revert`/`reset` 判的仍是 store 的 `enabled`
 * —— 用本 hook 去关那些，会让已暂存的条目在核停时突然撤销不掉。
 */

import { stagingActive } from '@/lib/staged-config';
import { useAppStore } from './app-store';
import { useStagedConfigStore } from './staged-config-store';

export function useStagingActive(): boolean {
  const enabled = useStagedConfigStore((s) => s.enabled);
  const stagedCount = useStagedConfigStore((s) => s.entries.length);
  const coreRunning = useAppStore((s) => s.proxyStatus?.running ?? false);
  return stagingActive(enabled, coreRunning, stagedCount);
}
