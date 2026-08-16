/**
 * 订阅更新进度（`subscriptionId → 当前状态`）的**单一真值** store。
 *
 * # 为什么是 store + 窗口级订阅，而不是 NodesScreen 的 useState
 *
 * 同 `use-latency-store.ts` 已登记的那条教训：`ScreenRouter` 是裸 `switch`（无 keep-alive）⇒
 * 组件私有 state + 组件内订阅 = **切屏即卸载即丢**。订阅更新恰恰是「点下去等十几秒」的操作，
 * 用户在这十几秒里去首页看连接态是最常见的行为，切回来进度就没了。
 *
 * 另有一条本模块独有的理由：**后台 scheduler 也会更新订阅**（`runtime/subscription_scheduler.rs`
 * 与手动刷新共用 `perform_subscription_update`）。没有人「发起」它，所以不存在一个可以挂
 * `useState(true)` 的调用点——自动刷新期间节点数会在用户眼皮底下变，而界面此前对此零交代。
 * 事件流是唯一能同时覆盖两条腿的信道。
 *
 * # 失败态**留在栏上**，成功态**清掉**
 *
 * - `done` / `unchanged` → 删除条目。结果已经写在原地了：节点计数变了、「更新时间」变成「刚刚」，
 *   外加一条三态 toast。再挂一个「已完成」徽标是同一事实的第三份低精度副本。
 * - `failed` → **保留**，直到下一次更新开始（`fetching` 覆盖）或成功（删除）。
 *   判据：toast 2.2s 就散了，而「这条订阅现在是停更的」是一个**持续为真的状态**，
 *   不是一个瞬时事件。后台 scheduler 的失败尤其如此——用户根本不在场，toast 白弹。
 *
 * # 不落 localStorage（会话内存态）
 *
 * 同 `use-latency-store` 的分界：这是**观测**不是偏好。重启后「上次失败」既无从复核也无从重试
 * （用户不知道是几天前的），留着即误导。
 *
 * # 没有静默超时兜底 —— 终态由**后端结构**保证
 *
 * `speedtest-progress-toast.ts` 需要 `SPEEDTEST_IDLE_TIMEOUT_MS` 是因为那边一轮测速有多条腿、
 * 每条腿各自有出口。这里后端 `perform_subscription_update` 是一层薄壳：内层无论从哪条 `return`
 * 出来，外层都从返回值派生终态帧（源码门 `the_update_shell_cannot_return_without_a_terminal_frame`
 * 钉住）。剩下的唯一漏帧形态是「后端进程没了」，而那时整个窗口一起完蛋，一个前端定时器救不了。
 * 与其加一个永远不该触发、触发了也只会把活着的更新误判成失败的定时器，不如不加。
 */
import { create } from 'zustand';
import { api } from '@/ipc';
import {
  isSubscriptionUpdateTerminal,
  type SubscriptionUpdateProgress,
} from '@/contracts/subscription-progress';
import { retainRecordKeys } from './retain-entities';

/** 订阅信息栏要显示的东西。`null` = 该订阅当前无事发生（既不在更新，也没有未清的失败）。 */
export type SubscriptionProgressEntry = SubscriptionUpdateProgress | null;

/** `subscriptionId → 当前状态`。键缺席 = 无事发生。 */
export type SubscriptionProgressMap = Record<string, SubscriptionUpdateProgress>;

/**
 * 收到一帧 → 新的 map（**纯函数**，store 之外可直接单测）。
 *
 * 三条规则（改任意一条都会被 `use-subscription-progress-store.test.ts` 抓到）：
 *  1. `done` / `unchanged` → 删键（顺带清掉上一次残留的失败徽标）；
 *  2. `failed` → 写入并**长期保留**；
 *  3. 过程帧 → 覆盖写入（新一轮的 `fetching` 因此天然顶掉上一轮的失败）。
 */
export function reduceSubscriptionProgress(
  map: SubscriptionProgressMap,
  ev: SubscriptionUpdateProgress
): SubscriptionProgressMap {
  // 无归属的帧一律丢弃：写进 map 会以空键挂住，且没有任何一条订阅栏读得到它。
  if (!ev.subscriptionId) return map;

  if (isSubscriptionUpdateTerminal(ev.phase) && ev.phase !== 'failed') {
    if (!(ev.subscriptionId in map)) return map; // 已经是空的 → 不制造新对象引用（省一次重渲染）
    const next = { ...map };
    delete next[ev.subscriptionId];
    return next;
  }
  return { ...map, [ev.subscriptionId]: ev };
}

interface SubscriptionProgressState {
  progress: SubscriptionProgressMap;
  /** 收到一帧（事件流的**唯一**写入口——没有手动 dismiss：失败徽标该由「重试成功」清掉，
   *  而不是由「假装没看见」清掉。 */
  applyProgress: (ev: SubscriptionUpdateProgress) => void;
  retainSubscriptionIds: (subscriptionIds: readonly string[]) => void;
}

export const useSubscriptionProgressStore = create<SubscriptionProgressState>((set) => ({
  progress: {},
  applyProgress: (ev) => set((s) => ({ progress: reduceSubscriptionProgress(s.progress, ev) })),
  retainSubscriptionIds: (subscriptionIds) =>
    set((state) => {
      const progress = retainRecordKeys(state.progress, new Set(subscriptionIds));
      return progress === state.progress ? state : { progress };
    }),
}));

/** 取某条订阅的当前状态（组件里用：`useSubscriptionProgress(sub.id)`）。 */
export function useSubscriptionProgress(subscriptionId: string): SubscriptionProgressEntry {
  return useSubscriptionProgressStore((s) => s.progress[subscriptionId] ?? null);
}

/**
 * 把 `event:subscriptionUpdateProgress` 接到本 store —— **窗口级持久订阅**，返回退订函数。
 *
 * 调用点必须是该窗口生命周期内只挂载一次的根（主窗 `App.tsx`）。挂进 NodesScreen 即退回
 * 「切屏即丢」，也就是本模块存在的那个缺陷本身。托盘浮层不订：它不显示订阅信息栏。
 */
export function subscribeSubscriptionProgressEvents(): () => void {
  return api.subscription.onUpdateProgress((ev) => {
    useSubscriptionProgressStore.getState().applyProgress(ev);
  });
}
