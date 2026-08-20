/**
 * 节点列表「按延迟排序」开关的 localStorage 持久 store（Polaris / Tauri 移植自 Polaris）。
 *
 * 为何独立 store + localStorage（同 use-tailscale-login-cache-store 惯例）：
 *  - 这是「展示偏好」而非「易变数据」：用户设一次该跨重启保留（与「测速结果只活在应用生命周期、重启清空」
 *    不冲突——结果是数据、排序是偏好）。
 *  - 存 localStorage 而非 UserConfig：UserConfig 改动触发 CONFIG_CHANGED→重启代理；偏好是纯渲染端视图态。
 *
 * 开关管理「Nodes 工具栏 + 首页出口下拉 + 托盘列表」的排序，三处共读本 store（原型 :4475
 * "single source of truth ... persisted + tray-synced"）。无测速结果时一律退化为按名称（恒沉底，不随方向翻转）。
 *
 * # 托盘怎么同步（**与 上游 不同，别照搬**）
 *
 * 上游的托盘是**原生菜单**，菜单状态在主进程 TrayManager 手里 → 必须经 IPC 把开关推过去。
 * Polaris 的托盘是**独立 webview 浮层**（`tray.html`，与主窗 `index.html` 同为 `WebviewUrl::App`
 * = 同源）→ **localStorage 天然共享**，托盘端直接读本 store 即可，无需 IPC 绕一圈。
 *
 * 故排序**不设后端 command**：原 `api.config.setNodeSortByLatency` + `app_set_node_sort_by_latency`
 * 是架构变更后的空转桩（参数从不使用、前端零调用），已随本轮清理删除（复审队列 R13）。
 */
import { create } from 'zustand';

// Polaris 命名空间（与 Polaris 的 'polaris.nodeSortByLatency' 区分，避免迁移期双装共用残留）
const STORAGE_KEY = 'polaris.nodeSortByLatency';

// 导出供单测直接验证。
export function loadNodeSortByLatency(): boolean {
  try {
    return localStorage.getItem(STORAGE_KEY) === 'true';
  } catch {
    /* 无 localStorage / 隐私模式 → 默认按名称 */
    return false;
  }
}

function persist(value: boolean): void {
  try {
    localStorage.setItem(STORAGE_KEY, value ? 'true' : 'false');
  } catch {
    /* 持久化失败不阻断 → 退化为会话内记忆 */
  }
}

interface NodeSortState {
  /** true=按延迟排序，false=按名称（默认）。 */
  sortByLatency: boolean;
  setSortByLatency: (value: boolean) => void;
  toggleSortByLatency: () => void;
}

export const useNodeSortStore = create<NodeSortState>((set, get) => ({
  sortByLatency: loadNodeSortByLatency(),
  setSortByLatency: (value) => {
    if (get().sortByLatency === value) return; // 值未变，省写 + 省订阅者重渲染
    persist(value);
    set({ sortByLatency: value });
  },
  toggleSortByLatency: () => get().setSortByLatency(!get().sortByLatency),
}));

/**
 * 跨窗同步：`storage` 事件（同源**其它** document 写 localStorage 时派发）→ 回灌本窗 store。
 *
 * # 为什么非要有它（上面那段"天然共享"只对了一半）
 *
 * localStorage 的**存储**确实跨窗共享，但每个 webview 各有一份 zustand store 实例，而 store 只在
 * **创建时**读一次 `loadNodeSortByLatency()`。托盘浮层按需创建，隐藏后仍会保留至 120s 回收；在这一代
 * warm WebView 存活期间，用户若在主窗切换「按延迟排序」，主窗和 localStorage 已是新值，但浮层这份
 * store 不会自行重读。回收重建能碰巧刷新，却不能让正确性依赖等待 120s 或重启 App。
 *
 * `storage` 事件恰好只在**别的 document**写入时派发（写入方自己不收到），语义正是我们要的"别处改了"，
 * 且无需任何 IPC 通道。
 *
 * # 形态
 *
 * 处理函数 [`applyNodeSortStorageEvent`] 导出供单测直调（不 mock 全局事件也能验判定）；模块底部的
 * `addEventListener` 是**模块级副作用**——本 store 被主窗与浮层双方 import，订阅随之在两侧自动挂上，
 * 不需要任何调用点记得去"初始化"一下（那种要人记得调的接线，正是最容易在新窗口里漏掉的东西）。
 */
export function applyNodeSortStorageEvent(event: Pick<StorageEvent, 'key' | 'newValue'>): boolean {
  // key===null = storage.clear()：整片被清 → 回落默认（按名称），与 loadNodeSortByLatency 的兜底同口径。
  if (event.key !== null && event.key !== STORAGE_KEY) return false;
  const next = event.key === null ? false : event.newValue === 'true';
  const store = useNodeSortStore.getState();
  if (store.sortByLatency === next) return false;
  // **不走 setSortByLatency**：那会 persist 一次，把刚从别处收到的值再写回去（自己触发自己的回声）。
  useNodeSortStore.setState({ sortByLatency: next });
  return true;
}

if (typeof window !== 'undefined' && typeof window.addEventListener === 'function') {
  window.addEventListener('storage', (e) => {
    applyNodeSortStorageEvent(e);
  });
}
