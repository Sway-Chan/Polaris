/**
 * 内置应用分流预设表的 store（Rust SoT 的前端缓存）。
 *
 * 表本体在 Rust（`config-engine/user_config/app_rules_preset_data.rs`），经 `app_presets_list`
 * 拉取。**常量表 + 启动时一次 invoke 入 store** 是 §D Q3 的定式（表数据 KB 级，一次往返摊销为零）；
 * 不逐次 invoke —— 渲染是同步的，卡片墙 per-row 取预设必须同步命中。
 *
 * 为何独立 store 而非塞进 app-store：
 *  - 生命周期不同。app-store 的 config 随 `config:changed` 反复重载；本表是**进程内不变量**
 *    （Rust 静态表），拉一次即可，不该跟着 config 风暴重取。
 *  - app-store 已 400+ 行且是并发批次的热点文件，独立 slice 冲突面更小（同 use-node-sort-store 惯例）。
 *
 * **不持久化到 localStorage**：表随版本走（升级即可能增删预设），缓存到磁盘只会让旧表盖新表。
 * 一次 invoke 的代价远小于「陈旧预设表」的排查成本。
 */
import { create } from 'zustand';
// 直连 ipc-client 的 invoke 而非 api-client 的命名空间方法：`ui/src/ipc/` 当前由并发批次占用，
// 本批不改该目录。**仍走 invoke 这个全链路唯一拆信封点**（ApiResponse → data / 失败抛 IpcError），
// 不是绕过它（§N3 的三处 `import { invoke } from '@tauri-apps/api/core'` 裂口才是绕过）。
// 后续 ipc/ 空闲时应收编为 `api.appPresets.list()`，见报告「移交事项」。
import { invoke } from '@/ipc/ipc-client';
import { IPC_CHANNELS } from '@/domain/ipc-channels';
import type { AppPreset } from '@/domain/app-rules-preset';

interface AppPresetsState {
  /** Rust 下发的内置预设表（未合并自定义 —— 合并见 mergeAppPresets）。 */
  presets: AppPreset[];
  loading: boolean;
  /** 拉取失败的原文（IpcError.message）。非 null 时卡片墙只剩自定义应用。 */
  error: string | null;
  /** 已成功拉过一次（区分「空表」与「还没拉」）。 */
  loaded: boolean;
  loadPresets: () => Promise<void>;
}

// 单飞：多个组件同时 mount 时只发一次 invoke（同 app-store 的 loadConfig 单飞惯例）。
let inflight: Promise<void> | null = null;

export const useAppPresetsStore = create<AppPresetsState>((set, get) => ({
  presets: [],
  loading: false,
  error: null,
  loaded: false,

  loadPresets: async () => {
    // 静态表：已拉到就不再拉（失败过则允许重试）。
    if (get().loaded) return;
    if (inflight) return inflight;

    set({ loading: true, error: null });
    inflight = (async () => {
      try {
        const presets = await invoke<AppPreset[]>(IPC_CHANNELS.APP_PRESETS_LIST);
        // 防御：后端契约是数组；非数组说明信封/契约出问题，宁可报错也不要把 undefined 灌进渲染层
        // （§N3 的病根正是「坏值当好值用」→ 渲染期崩）。
        if (!Array.isArray(presets)) {
          throw new Error(`APP_PRESETS_INVALID_RESPONSE:${typeof presets}`);
        }
        set({ presets, loaded: true, loading: false, error: null });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        console.error('[app-presets] 内置预设表拉取失败:', err);
        // 不抛：预设表拉不到时应用分流屏应能降级渲染（自定义应用仍在 config 里），
        // 而不是整屏炸掉。error 置位供 UI 提示。
        set({ loading: false, error: message });
      } finally {
        inflight = null;
      }
    })();
    return inflight;
  },
}));
