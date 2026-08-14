/**
 * 测速结果（`serverId → 延迟 ms`）的**单一真值** store。
 *
 * # 为什么必须是 store 而不是组件 useState
 *
 * 迁移期这份 map 曾以**四份组件私有 `useState`** 存在（HomeScreen / NodesScreen / StatusBar / TrayMenu），
 * 各自在自己的 `useEffect` 里订 `onSpeedTestResult`。后果是订阅寿命 = 组件挂载寿命：
 * `ScreenRouter` 是裸 `switch`（无 keep-alive）⇒ **切屏即卸载即丢结果**，切回来一片空白；
 * 且测速期间切走，流式结果无人接收。
 *
 * 上游 明确修过同一个 bug —— `use-native-events.ts:371-374`：
 * >「原 per-node 监听写在 handler 作用域内…提到顶层持久 hook 后跨页不丢」
 *
 * 故本 store 的契约有两条，缺一条都会退回原状：
 *  1. 状态**存在组件外**（本 store）；
 *  2. 订阅**挂在窗口级持久位置**（主窗 `App.tsx`、托盘窗 `TrayMenu` 根），经 [`subscribeLatencyEvents`]。
 *
 * # 不持久化（会话内存态）
 *
 * 延迟是**测量数据**不是偏好：重启后旧值不再代表当前网络，留着即误导。
 * 对比 `use-node-sort-store`（排序开关=偏好 → localStorage 持久），二者刻意不同，勿照抄。
 *
 * # 托盘窗是**独立 JS 堆**（别指望跨窗共享）
 *
 * `tray.html` 是独立 Vite 入口 / 独立 webview ⇒ 与主窗同源（localStorage 共享）但**不共享 JS 堆**，
 * 本 store 在托盘窗里是**另一个实例**。这不是缺陷：托盘自订同一事件流，两窗各自收敛到同一后端真值
 * （事件由 `commands/speedtest.rs` 向全部窗口广播）。跨窗共享内存态需 localStorage/IPC，
 * 而延迟按上一节的理由不该落 localStorage。
 */
import { create } from 'zustand';
import { api } from '@/ipc';

/**
 * 「延迟已陈旧」阈值（契约「状态栏」：`当前节点延迟(陈旧>30min 半透明)`）。
 *
 * 为什么必须配一份 `testedAt` 而不是只存数值：延迟是**有保质期的测量**，30 分钟前测出的 42ms 与刚测出的
 * 42ms 在屏幕上一模一样，但对用户的行动含义完全不同（后者可信、前者该重测）。没有时间戳就无从分辨，
 * 半透明这条契约也就无数据基础。
 */
export const LATENCY_STALE_MS = 30 * 60 * 1000;

/** 该节点的延迟是否已陈旧（无时间戳=从未测过 → 不算陈旧，由「—」自己表达「没有值」）。 */
export function isLatencyStale(testedAt: number | undefined, now: number = Date.now()): boolean {
  if (testedAt === undefined) return false;
  return now - testedAt > LATENCY_STALE_MS;
}

interface LatencyState {
  /** `serverId → 延迟 ms`。`null`=后端真实探测失败/超时；键缺席=本会话未测过（UI 显「—」）。 */
  latencyMap: Record<string, number | null>;
  /**
   * `serverId → 该延迟的落库时刻（epoch ms）`。与 `latencyMap` **同写同键**：两个写入口
   * （`applyLatencyResult` / `applyLatencyResults`）就是全部写入点，故只要打戳收口在本 store 内，
   * 就不存在「某条写入路径忘了打戳」的漏面——组件侧无法绕过它直接改 map。
   */
  testedAt: Record<string, number>;
  /** 单节点结果落库（`onSpeedTestResult` 流式回填的唯一写入口）。 */
  applyLatencyResult: (serverId: string, latency: number | null) => void;
  /** 批量合并（invoke 返回值兜底同步：事件丢失时补齐）。**合并不替换** —— 未在本批的节点保留历史值。 */
  applyLatencyResults: (results: Record<string, number | null>) => void;
}

/**
 * IPC 用 `-1` 表示「真测了但失败」，渲染域统一用 `null` 表示同一语义。
 *
 * 归一化必须收在 store 的两个写入口，而不是让四个消费方各自记得处理：漏掉 NodeCard 的旧形态会把
 * `-1` 同时渲染成绿色（`latLevel(-1) < 80`）和文字 “-1 ms”，正是用户看到的矛盾状态。
 */
export function normalizeLatencyResult(latency: number | null): number | null {
  return latency === null || !Number.isFinite(latency) || latency < 0 ? null : latency;
}

export const useLatencyStore = create<LatencyState>((set) => ({
  latencyMap: {},
  testedAt: {},
  applyLatencyResult: (serverId, latency) =>
    set((s) => ({
      latencyMap: { ...s.latencyMap, [serverId]: normalizeLatencyResult(latency) },
      testedAt: { ...s.testedAt, [serverId]: Date.now() },
    })),
  applyLatencyResults: (results) =>
    set((s) => {
      // 一批同一时刻：批量结果来自同一次 speedTest 的 invoke 返回值，逐键取 Date.now() 只会引入
      // 无意义的毫秒级抖动。
      const now = Date.now();
      const stamps: Record<string, number> = { ...s.testedAt };
      const normalized: Record<string, number | null> = {};
      for (const [id, latency] of Object.entries(results)) {
        stamps[id] = now;
        normalized[id] = normalizeLatencyResult(latency);
      }
      return { latencyMap: { ...s.latencyMap, ...normalized }, testedAt: stamps };
    }),
}));

/**
 * 把 `onSpeedTestResult` 事件流接到本 store —— **窗口级持久订阅**，返回退订函数。
 *
 * 调用点必须是该窗口生命周期内**只挂载一次**的根（主窗 `App.tsx` / 托盘窗 `TrayMenu`）。
 * 挂到业务组件里即退回「切屏即丢」（见本文件顶部）。
 */
export function subscribeLatencyEvents(): () => void {
  return api.server.onSpeedTestResult((r) => {
    useLatencyStore.getState().applyLatencyResult(r.serverId, r.latency);
  });
}
