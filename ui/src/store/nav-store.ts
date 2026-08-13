/**
 * 导航 store（Zustand）—— 主屏活动页 + 侧栏折叠态 + settings 二级 tab。
 *
 * 与 app-store 解耦：app-store 持业务态（config / proxy / 节点），纯 UI 导航态放这里，
 * 随组件阶段补（侧栏折叠、settings 子页、返回应用等）。原型 [data-nav] 七项 + settings 八项
 * （见 polaris-prototype.html L1588-1626），但当前批次仅占位，screen 路由按 mainScreen 渲染空壳。
 *
 * 折叠态持久化到 localStorage（与 i18n 同款 try/catch 兜底）。
 */

import { create } from 'zustand';
import { IPC_CHANNELS } from '@/domain/ipc-channels';

/** 主屏导航项（对齐原型 .nav-item[data-nav]）。settings 走独立 scope。 */
export type MainScreen =
  | 'home'
  | 'nodes'
  | 'rules'
  | 'apppolicy'
  | 'resources'
  | 'connections'
  | 'logs';

/** settings 子页（对齐原型 .set-nav[data-set]）。 */
export type SettingsScreen =
  | 'general'
  | 'display'
  | 'network'
  | 'dns'
  | 'tun'
  | 'update'
  | 'backup'
  | 'helper'
  | 'about';

/** 顶层 scope：主应用或设置子界面。 */
export type NavScope = 'main' | 'settings';

interface NavState {
  scope: NavScope;
  mainScreen: MainScreen;
  settingsScreen: SettingsScreen;
  sidebarCollapsed: boolean;

  navigate: (screen: MainScreen) => void;
  enterSettings: (screen?: SettingsScreen) => void;
  setSettingsScreen: (screen: SettingsScreen) => void;
  backToApp: () => void;
  toggleSidebar: () => void;
  setSidebarCollapsed: (collapsed: boolean) => void;
}

const SIDEBAR_KEY = 'polaris.sidebar.collapsed';

function readSidebarCollapsed(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_KEY) === '1';
  } catch {
    return false;
  }
}

function persistSidebarCollapsed(collapsed: boolean): void {
  try {
    localStorage.setItem(SIDEBAR_KEY, collapsed ? '1' : '0');
  } catch {
    /* localStorage 不可用：退化为会话内记忆 */
  }
}

export const useNavStore = create<NavState>((set) => ({
  scope: 'main',
  mainScreen: 'home',
  settingsScreen: 'general',
  sidebarCollapsed: readSidebarCollapsed(),

  navigate: (screen) => set({ scope: 'main', mainScreen: screen }),
  enterSettings: (screen) =>
    set({ scope: 'settings', settingsScreen: screen ?? 'general' }),
  setSettingsScreen: (screen) => set({ settingsScreen: screen }),
  backToApp: () => set({ scope: 'main' }),

  toggleSidebar: () =>
    set((state) => {
      const collapsed = !state.sidebarCollapsed;
      persistSidebarCollapsed(collapsed);
      return { sidebarCollapsed: collapsed };
    }),
  setSidebarCollapsed: (collapsed) => {
    persistSidebarCollapsed(collapsed);
    set({ sidebarCollapsed: collapsed });
  },
}));

/* ──────────────────────────────────────────────────────────────────────────────
 * 托盘「打开设置」的**主窗侧消费点**（A1）
 *
 * 托盘浮层是独立窗口、独立 JS 堆，够不着本 store；而设置屏只存在于主窗。后端因此给了一条**窄通道**：
 * `tray_show_main` 的受限目标屏参数 → `event:trayOpenScreen`（值域由 Rust 白名单
 * `normalize_tray_screen` 钉死，当前仅 `'settings'`）。这不是已删除的 `EVENT_NAVIGATE` 通用路由的
 * 复活——选型理由见 `src-tauri/src/events.rs` 该常量上方与 `tray.rs::normalize_tray_screen`。
 *
 * 两条投递腿，对应主窗的两种状态：
 *  1. **事件腿**：主窗存在（含 close-to-tray 隐藏态）→ 下面的 `listen` 收。
 *  2. **首帧种子腿**：主窗已被 C16 轻量模式销毁 → 后端重建它时把目标屏注入
 *     `window.__POLARIS_TRAY_SCREEN__`，事件腿此刻必丢（订阅还没挂上）。
 * 「轻量模式 → 托盘 → 打开设置」是浮层里挨着的两个按钮，不是理论路径，故两条都要有。
 * ────────────────────────────────────────────────────────────────────────────── */

/** 窄通道的目标屏 → 导航动作。未登记值一律忽略（**不猜**：宁可不跳，也不跳错屏）。 */
export function applyTrayScreenIntent(screen: unknown): boolean {
  if (screen !== 'settings') return false;
  useNavStore.getState().enterSettings();
  return true;
}

declare global {
  interface Window {
    /** 后端首帧种子（`tray::tray_screen_boot_script`）。仅主窗重建腿会有值。 */
    __POLARIS_TRAY_SCREEN__?: string;
  }
}

/**
 * 后端 `tray_take_pending_screen` 的取货 + 「消费后清」。
 *
 * 两个调用时机，同一个动作：
 *  1. **装配时取一发** —— 覆盖「窗在、但 webview 还没挂上订阅」那段：那时事件腿 emit 出去没人听，
 *     种子腿也不会再注入（窗早建好了）⇒ intent 静默丢失（窗开了、屏没跳）。这条腿把它补上。
 *  2. **事件腿命中之后再调一次** —— `tray_show_main` 现在**恒写** pending，事件腿处理掉的那一路
 *     会在后端留下一份余量；不清的话它会以陈旧意图的形式活到下一次建窗（重建即跳设置页）。
 *
 * 失败静默：非 Tauri（浏览器预览/测试）没有这个 command，另两条腿不受影响。
 */
async function drainTrayPendingScreen(apply: boolean): Promise<void> {
  try {
    const { invoke } = await import('@/ipc/ipc-client');
    // 命令名字面量（托盘族 command 全在 `tray.rs`、不走 `api-client`，与 `TrayMenu.tsx` 里
    // `invoke('tray_show_main')` 同款写法）。ipc-client 已拆 ApiResponse 信封 → 直接是目标屏。
    const screen = await invoke<string | null>(IPC_CHANNELS.TRAY_TAKE_PENDING_SCREEN);
    if (apply && screen != null) applyTrayScreenIntent(screen);
  } catch {
    /* 非 Tauri / 命令不可达：另两条腿仍在 */
  }
}

/**
 * 装上三条腿。模块级自执行（本 store 被主窗侧栏/AppShell import，订阅随之自动挂上）——
 * 刻意不做成"某处记得调一次 init"：那种接线正是最容易在窗口重建路径上漏掉的东西，而漏掉后
 * 表现为「点了打开设置，窗开了、屏没跳」这种**静默**半失效。
 *
 * 三条腿各自覆盖主窗的一种状态，理由见 `src-tauri/src/tray.rs::tray_show_main`。
 *
 * 托盘浮层不 import 本 store（`TrayMenu.tsx` 只用 use-node-sort/use-latency），故不会在浮层里空转。
 */
if (typeof window !== 'undefined') {
  // ① 首帧种子腿：读一次即清（`delete` 后再触发的重建不会重复跳屏）。
  const seeded = window.__POLARIS_TRAY_SCREEN__;
  if (seeded !== undefined) {
    delete window.__POLARIS_TRAY_SCREEN__;
    applyTrayScreenIntent(seeded);
  }
  // ② 取货腿：装配即取一发。种子腿已经跳过屏时**只清不跳**（同一条 intent 不跳两次）。
  void drainTrayPendingScreen(seeded === undefined);
  // ③ 事件腿：动态 import 避免把 Tauri event API 拉进非 Tauri 预览态的同步依赖链（失败即静默降级，
  //    只剩另两条腿——与浮层里所有 invoke 的 `.catch(() => {})` 同款「非 Tauri 不报错」取向）。
  void import('@tauri-apps/api/event')
    .then(({ listen }) =>
      listen<string>(IPC_CHANNELS.EVENT_TRAY_OPEN_SCREEN, (e) => {
        applyTrayScreenIntent(e.payload);
        // 消费后清：把 `tray_show_main` 恒写下的那份余量取走丢弃（不再 apply，已经跳过了）。
        void drainTrayPendingScreen(false);
      })
    )
    .catch(() => {
      /* 非 Tauri（浏览器预览 / 测试）：无事件通道，另两条腿仍在 */
    });
}
