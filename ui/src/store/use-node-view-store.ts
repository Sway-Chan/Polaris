/**
 * 节点列表「卡片 / 列表」视图档的 localStorage 持久 store。
 *
 * 为何从 `NodesScreen` 的 `useState('cards')` 挪出来（同 `use-node-sort-store` 的既有理由）：
 *  - `ScreenRouter` 是裸 switch，切屏即卸载重挂 → 局部 state 每次回默认「卡片」。用户在列表视图下
 *    去设置页调个开关再回来，视图被悄悄改回去了，这不是「重置」是**丢用户设置**。
 *  - 这是「展示偏好」而非「易变数据」：设一次该跨重启保留，故 localStorage 而非内存。
 *  - 存 localStorage 而非 UserConfig：UserConfig 改动会触发 CONFIG_CHANGED → 重启代理；
 *    视图档是纯渲染端视图态，不该让切个列表视图把代理重启一遍。
 *
 * 写法（读取容错 / 写失败降级为会话内记忆 / 值未变不写）1:1 照 `use-node-sort-store`，
 * 两者是同类偏好，别各写一套。
 */
import { create } from 'zustand';

/** 节点列表视图档。'cards'=卡片网格（默认，原型初始态），'list'=紧凑列表（`#s-nodes.nodes-list-view`）。 */
export type NodeViewMode = 'cards' | 'list';

const STORAGE_KEY = 'polaris.nodeViewMode';
const DEFAULT_VIEW: NodeViewMode = 'cards';

// 导出供单测直接验证。
export function loadNodeViewMode(): NodeViewMode {
  try {
    return localStorage.getItem(STORAGE_KEY) === 'list' ? 'list' : DEFAULT_VIEW;
  } catch {
    /* 无 localStorage / 隐私模式 → 默认卡片视图 */
    return DEFAULT_VIEW;
  }
}

function persist(value: NodeViewMode): void {
  try {
    localStorage.setItem(STORAGE_KEY, value);
  } catch {
    /* 持久化失败不阻断 → 退化为会话内记忆 */
  }
}

interface NodeViewState {
  view: NodeViewMode;
  setView: (value: NodeViewMode) => void;
}

export const useNodeViewStore = create<NodeViewState>((set, get) => ({
  view: loadNodeViewMode(),
  setView: (value) => {
    if (get().view === value) return; // 值未变，省写 + 省订阅者重渲染
    persist(value);
    set({ view: value });
  },
}));
