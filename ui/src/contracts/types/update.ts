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

/**
 * 弹窗阶段（= Rust `PopupPhase`，serde 小写）。
 *
 * `noupdate` 是本移植新增的第五档（上游只有四档）：用户点了「更新」，复查回来却没有任何可下载的
 * 包。它此前借用 `done` 收场 —— 弹窗于是渲染「下载完成」+ 满格进度条，而一个字节都没下。
 * 成因与「为什么不塞进现有四档」见 `crates/updater/src/state.rs` 的 `PopupPhase::NoUpdate`。
 *
 * 本联合与 Rust 枚举由 `ui/src/lib/update-popup-action-parity.test.ts` 逐字对拍。
 */
export type PopupPhase = 'remind' | 'progress' | 'done' | 'noupdate' | 'error';

/**
 * 弹窗状态载荷（主 → 弹窗；= Rust `UpdatePopupState`，serde camelCase）。
 *
 * 字段集与 Rust 结构体由 `ui/src/contracts/update-popup-state-parity.test.ts` 双向对拍
 * （少一个字段在这里长得跟「后端这一帧没给」一模一样，两个编译器都不会说话）。
 */
export interface UpdatePopupState {
  phase: PopupPhase;
  /** 目标新版本号（remind / done / noupdate 态；会话级继承，见 Rust `PopupSession::send_state`）。 */
  version?: string;
  /** 当前版本号（remind 态）。 */
  currentVersion?: string;
  /** 下载进度百分比 0-100（progress/done 态）。 */
  percentage?: number;
  /** 本次下载已收字节（progress 态；回调原值，不是从百分比反推）。 */
  receivedBytes?: number;
  /** 本次下载总字节（progress 态；= 清单 `fileSize`。缺失 = 分母未知，**不得凑**）。 */
  totalBytes?: number;
  /** 是否走镜像下载（progress 态角标）。⚠️ 后端今天无生产写点，见 Rust 侧待修表。 */
  mirror?: boolean;
  /** 包的落位路径（done 态；Rust 侧是 `done()` 的必填参数 —— 没有它就不是「下完了」）。 */
  filePath?: string;
  /** 失败机器码（error 态；= Rust `UpdateErrCode::wire()`，覆盖门对拍）。 */
  errorCode?: string;
  /** 技术诊断串（error 态；语言中性数据：路径 / 哈希 / OS 错误原文，渲染端括注）。 */
  errorDetail?: string;
}

/** Rust `UpdateErrCode::wire()` 的全联合（U1）。覆盖门与两张 i18n 表都对拍它。 */
export type UpdateErrWire =
  | 'missingDownloadUrl'
  | 'digestFieldInvalid'
  | 'cacheDirFailed'
  | 'downloadFailed'
  | 'backendUnavailable'
  | 'downloadTaskFailed'
  | 'sizeMismatch'
  | 'digestHexInvalid'
  | 'digestMismatch'
  | 'landingFailed'
  | 'recheckFailed';

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
