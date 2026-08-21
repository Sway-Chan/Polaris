/**
 * 系统代理**活态**（`system_proxy_get_status` 的 `pointsToUs`）的**单一真值** store + 唯一轮询驱动。
 *
 * # 为什么必须是共享层，而不是各组件自己一个 hook
 *
 * 消费方现在有两个（`StatusBar` 的状态点/文案、`HomeScreen` 的 meta 行 + 降级横幅），且两者**同屏共存**
 * ——主内容区渲染 HomeScreen 的同时底部就挂着 StatusBar。若各自起一份 `useSystemProxyLive`：
 *  1. **开销翻倍**：每次查询后端都要 exec `networksetup`（mac 上 1 次列服务 + 3 次读协议，实测 ~34ms/次）
 *     / `gsettings` / `reg`，两份轮询就是双倍进程创建；
 *  2. **两处可能不同相**：两条链的定时器起点不同（HomeScreen 随路由挂载、StatusBar 随布局挂载），
 *     同一时刻可能一处已判 `not-effective`、另一处还停在上一拍的 `effective` ⇒ 首页说「未生效」、
 *     状态栏还亮着绿灯，正是本功能要根治的那种自相矛盾。
 * 这条纪律由 `store/system-proxy-live-wiring.test.ts` 的源码不变量守卫钉死。
 *
 * # 为什么是 store 而不是 App.tsx 顶层 `useState` + prop 传递
 *
 * 两个消费方之间隔着 `AppShell` → `ScreenRouter`（裸 switch）两层，prop 传递要穿透两个与该数据毫不
 * 相干的中转组件，且都是别的批次在改的文件。store 让「谁需要谁自取」，接线面只有本文件 + App.tsx
 * 一行调用。这也与仓内既有形态一致：`use-latency-store`（测速结果）、`app-store.ipInfo`（出口 IP）
 * 都是「订阅/轮询挂 App.tsx 顶层、真值存 store、消费方各自读」。
 *
 * 独立成文件而非并进 `app-store`：`app-store` 是**后端真值的镜像**（config / proxyStatus / servers…），
 * 而本 store 是前端自己发起的**探测结果**——同 `use-latency-store` 的分类。
 *
 * # 托盘窗
 *
 * `tray.html` 是独立 webview / 独立 JS 堆，本 store 在那边是另一个实例、且**不调轮询驱动**
 * （常驻轮询在一个「弹出才可见、几秒就收起」的窗上纯烧 exec）。浮层改走 [`fetchSystemProxyLive`]：
 * 每次 hydrate（弹出获焦即触发）**取一发**，用完即弃、不写 store。
 * 这条腿是必需的：少了它，OS 代理被用户手改时主窗状态栏亮琥珀「未生效」、托盘同一时刻显绿点
 * 「已连接」——正是活态这个功能要根治的那类自相矛盾，只是换到了浮层与主窗之间（2026-07-28 复审）。
 *
 * `system-proxy-live-wiring.test.ts` 的「后端查询语句只出现在本文件一处」仍然成立且仍然是对的：
 * 所有取数都收在这里，消费方拿到的是**同一套判定**，只是常驻轮询与一次性取数两种节奏。
 */
import { useEffect } from 'react';
import { create } from 'zustand';
import { api } from '@/ipc';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import type { SystemProxyLive } from '@/components/screens/home/connection-state';

/**
 * 系统代理活态查询的轮询间隔。
 *
 * 定这个数的依据是**成本 × 事件性质**，不是「越快越好」：每次查询后端都会 exec
 * `networksetup` / `gsettings` / `reg`。而它要抓的事件——用户跑去系统设置里手动关掉/改掉代理——是
 * **分钟级的罕见人工操作**，不是数据面。15s 的发现延迟对这类事件完全够用，换来的是常驻期近乎为零的开销。
 */
export const SYSTEM_PROXY_POLL_MS = 15_000;

interface SystemProxyLiveState {
  /** 活态三态；`unknown` = 没拿到结论（不适用 / 读取受阻 / 尚未取到），由消费方回落 `errorCode` 腿。 */
  live: SystemProxyLive;
  setLive: (live: SystemProxyLive) => void;
}

const useStore = create<SystemProxyLiveState>((set) => ({
  live: 'unknown',
  setLive: (live) => set({ live }),
}));

/**
 * 消费方入口：读活态（`StatusBar` / `HomeScreen` 用这个，**不要**自己调
 * `api.proxy.getSystemProxyStatus`）。
 */
export function useSystemProxyLive(): SystemProxyLive {
  return useStore((s) => s.live);
}

/**
 * 本活态是否「适用」——**只有「核稳定运行、没有起核腿在飞，且接管方式是 systemProxy」时才有意义**：
 * TUN / manual 分支根本不消费这个信号（见 `deriveTakeoverConnState`），断开态更没有 mixed 口可比对。
 *
 * 兜底口径（config 未水合 → 按 `systemProxy`）**必须与 `deriveTakeoverConnState` 内那条一致**：
 * 两处若分叉，会出现「判定走了 systemProxy 分支、却因为这里判成非 systemProxy 而根本没去取活态」
 * 的常态未知。收在本函数里是为了让这条口径只有一份。
 */
export function isSystemProxyLiveApplicable(
  running: boolean,
  proxyModeType: string | undefined,
  starting = false
): boolean {
  return running && !starting && (proxyModeType ?? 'systemProxy') === 'systemProxy';
}

/**
 * **取一发**活态（不排期、不写 store）—— 给「弹出即刷新」型消费方（托盘浮层）用。
 *
 * 失败折 `unknown` 而非 `not-effective`：读不到 ≠ 没生效（详见 `connection-state.ts` 模块头）。
 * 调用方须自己先过 [`isSystemProxyLiveApplicable`] 闸门——不适用时根本不该发这个查询。
 */
export async function fetchSystemProxyLive(): Promise<SystemProxyLive> {
  try {
    const s = await api.proxy.getSystemProxyStatus();
    return s.pointsToUs ? 'effective' : 'not-effective';
  } catch {
    return 'unknown';
  }
}

/**
 * **唯一**的轮询驱动 —— 只许在窗口级持久位置（主窗 `App.tsx`）调用一次。
 *
 * 三层节流，缺一条就会变成常驻空转：
 *  1. `active` 门 —— 见 [`isSystemProxyLiveApplicable`]；退出适用范围立刻把结论丢回 `unknown`，
 *     别让陈旧的 `not-effective` 悬着。
 *  2. **窗口不可见即不查、也不排下一次**（`visibilityState`）—— 后台窗口没人看状态栏，查了纯烧 exec。
 *     链条在隐藏时自然停摆，由 `visibilitychange` 唤醒并**立刻补一发**（回到前台看到的是新鲜值，
 *     而不是等最多 15s）。这是「隐藏时不空转」的实现方式：不是把 setTimeout 留着空转判断，
 *     而是根本不留 timer。
 *  3. 单飞 `inFlight` —— 「可见性唤醒」与「定时到点」可能同时触发，不加这道会叠出两条轮询链。
 *
 * 与仓内 stats 那套可见性降流**互不相干**：那条走 topic 订阅 + worker 停机，这条只是一个定时器。
 *
 * 失败（核未运行 / 读取受阻 / 非 Tauri dev 环境）→ `unknown`，由 `deriveTakeoverConnState` 回落
 * `errorCode` 腿。**绝不折成 `not-effective`**：读不到 ≠ 没生效（详见 `connection-state.ts` 模块头）。
 */
export function useSystemProxyLivePolling(): void {
  const running = useAppStore((s) => s.proxyStatus?.running ?? false);
  const starting = useAppStore((s) => s.proxyStatus?.starting ?? false);
  const proxyModeType = useEffectiveConfig((c) => c?.proxyModeType);
  const active = isSystemProxyLiveApplicable(running, proxyModeType, starting);

  useEffect(() => {
    const setLive = useStore.getState().setLive;
    if (!active) {
      setLive('unknown');
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let inFlight = false;

    const tick = async () => {
      if (cancelled || inFlight) return;
      // 隐藏 → 直接返回且**不排期**（链条停摆，等 visibilitychange 唤醒）。
      if (document.visibilityState !== 'visible') return;
      inFlight = true;
      try {
        const s = await api.proxy.getSystemProxyStatus();
        if (!cancelled) setLive(s.pointsToUs ? 'effective' : 'not-effective');
      } catch {
        if (!cancelled) setLive('unknown');
      } finally {
        inFlight = false;
        // 排下一拍前**再判一次可见性**：窗口可能在 await 期间被收进托盘，那时排出去的 timer
        // 只会在 15s 后醒来空转一次再死掉。这里判掉，隐藏期就真的一个 timer 都不留。
        if (!cancelled && document.visibilityState === 'visible') {
          if (timer) clearTimeout(timer);
          timer = setTimeout(() => void tick(), SYSTEM_PROXY_POLL_MS);
        }
      }
    };

    const onVisibility = () => {
      if (document.visibilityState === 'visible') void tick();
    };
    document.addEventListener('visibilitychange', onVisibility);
    void tick();

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [active]);
}
