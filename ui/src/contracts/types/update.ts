/**
 * 更新域的前端类型（**按当前 Rust 契约重建**，非恢复被删的旧文件）。
 *
 * 单一真值在 Rust：
 *  - `UpdatePopupState` / `PopupPhase` → `crates/updater/src/popup.rs` + `state.rs`（serde camelCase，phase 小写）
 *  - `PopupAction`                     → `crates/updater/src/popup.rs`
 *  - 错误码                             → `src-tauri/src/commands/updater.rs`
 *
 * 审计 §E 的判定（勿推翻）：TS interface 与 Rust struct 是跨语言边界的**必要对应物**，不是造轮子——
 * 除非上 codegen（需引新依赖，已禁），否则必须手工保持同步。**改 Rust 侧字段务必同步本文件。**
 */

/** 弹窗阶段（= Rust `PopupPhase`，serde 小写）。 */
export type PopupPhase = 'remind' | 'progress' | 'done' | 'error';

/** 弹窗状态载荷（主 → 弹窗；= Rust `UpdatePopupState`，serde camelCase）。 */
export interface UpdatePopupState {
  phase: PopupPhase;
  /** 目标新版本号（remind 态）。 */
  version?: string;
  /** 当前版本号（remind 态）。 */
  currentVersion?: string;
  /** 下载进度百分比 0-100（progress/done 态）。 */
  percentage?: number;
  /** 已下载/总量文案（progress 态）。 */
  bytesText?: string;
  /** 是否走镜像下载（progress 态角标）。 */
  mirror?: boolean;
  /** 错误文案（error 态）。 */
  errorText?: string;
}

/** 弹窗动作（弹窗 → 主；= Rust `PopupAction`，serde camelCase）。 */
export type PopupAction =
  | 'update'
  | 'later'
  | 'skip'
  | 'viewLog'
  | 'cancel'
  | 'retry'
  | 'manualDownload'
  | 'close';

/** 内核构建来源（= Rust `CoreBuildKind`，serde 小写）。§C6 的判定产物。 */
export type CoreBuildKind = 'official' | 'fork' | 'unknown';

/** 版本变更通知（= Rust `PendingChangeNotice`）。show→ack 一次性。 */
export interface PendingChangeNotice {
  previousVersion: string;
  currentVersion: string;
}

/** `core_get_version_info` 的返回。 */
export interface CoreVersionInfo {
  currentVersion: string;
  bundledVersion: string;
  build: CoreBuildKind;
  hasBackup: boolean;
  backupVersion: string | null;
  pendingChangeNotice: PendingChangeNotice | null;
}

/** `version_get_info` 的返回。 */
export interface AppVersionInfo {
  appVersion: string;
  coreVersion: string;
  coreBaseline: string;
}

/**
 * 结构化错误码（`ApiResponse.code`；前端 `ipc-client` 在 `success:false` 时 throw `IpcError` 带此 code）。
 *
 * **为什么要有这些码**：这些路径此前返回 `{success:true, data:{ok:false}}` —— 前端不 throw，
 * 「功能根本没实现」与「一次业务性失败」长得一模一样（审计 §N4）。现按码区分，UI 可如实呈现「未接线」。
 */
export const UPDATE_ERROR_CODES = {
  /** HTTP/TLS 后端未接线 → 检查更新 / 下载全部不可达（依赖决策待用户拍板）。 */
  HTTP_BACKEND_UNAVAILABLE: 'HTTP_BACKEND_UNAVAILABLE',
  /** 换核落位链路未接线（需 helper 特权写 + ProxyRuntime/LifecycleGate 协同）。 */
  CORE_SWAP_NOT_WIRED: 'CORE_SWAP_NOT_WIRED',
  /** 无可回滚的内核备份。 */
  NO_CORE_BACKUP: 'NO_CORE_BACKUP',
} as const;

export type UpdateErrorCode = (typeof UPDATE_ERROR_CODES)[keyof typeof UPDATE_ERROR_CODES];

/** 主进程注入弹窗文档的初始态全局（= Rust `PopupBootstrap::init_script` 定义的变量）。 */
declare global {
  interface Window {
    __POLARIS_UPDATE_POPUP_INITIAL__?: UpdatePopupState;
  }
}
