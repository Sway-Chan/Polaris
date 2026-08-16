/**
 * Polaris IPC 客户端（Tauri 2 适配层）。
 *
 * 替换 Polaris 的 Electron ipcRenderer 封装。Tauri 2 的核心差异：
 *  - `invoke(cmd, args)` 返回 Promise<T>：Rust `#[tauri::command]` 的 Ok 值 → resolve，Err → reject。
 *    **但 Polaris 后端并不裸返 T**：`src-tauri/src/response.rs` 约定所有 command 统一返回
 *    `ApiResponse<T>` = `{ success, data?, error?, code? }` 信封（95/95 命令零例外，与 Electron 期
 *    registerIpcHandler 自封的信封逐字段一致）。故**本层负责拆信封**——这是全链路唯一的拆包点：
 *    `success:true` → resolve(data)；`success:false` → throw `IpcError`（带 error / code）。
 *    上层 api-client 的方法据此标注**解包后**的类型（`Promise<UserConfig>` 而非
 *    `Promise<ApiResponse<UserConfig>>`），组件直接消费 data，无需感知信封。
 *  - `listen(event, handler)` 返回 `Promise<UnlistenFn>`（异步！），handler 收 `(event) => event.payload`。
 *    Polaris 的 ipcClient.on 同步返回退订闭包；为保持前端调用不变（`const off = api.x.on(...); useEffect(() => off, [])`），
 *    本层用「立即返回退订闭包 + 内部异步登记 unlisten」模式：闭包内追到真 unlisten 再调。
 *
 * 浏览器/开发态（非 Tauri webview，如 `vite dev` 纯浏览器预览）：invoke/listen 不存在时回落 mock，
 * 避免底座在无 Tauri runtime 时整体崩（同 Polaris getIpcRenderer mock 思路）。
 */

import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { listen as tauriListen } from '@tauri-apps/api/event';
import type { UnlistenFn } from '@tauri-apps/api/event';
import type { ApiResponse } from '../contracts/types';
import i18n from '../i18n';

/** 是否运行在真 Tauri webview（有 __TAURI_INTERNALS__）。 */
function isTauri(): boolean {
  return (
    typeof window !== 'undefined' &&
    // Tauri 2 注入 __TAURI_INTERNALS__（api/event 经它调核心）
    '__TAURI_INTERNALS__' in window
  );
}

/**
 * 后端 `ApiResponse` 信封的失败态（`success:false`）→ 抛出本错误。
 *
 * 刻意**保留结构化 `code`**（后端 `ApiResponse.code`，如 ProxyErrorCode 串）而不压进 message：
 * 上层可按 `code` 做语言无关的分类；message 保留后端原文供日志与错误详情使用。
 */
export class IpcError extends Error {
  /** 后端结构化错误码（`ApiResponse.code`）；后端未给则 undefined。 */
  readonly code?: string;
  /** 触发失败的命令名，便于日志定位。 */
  readonly command: string;

  constructor(command: string, message: string, code?: string) {
    super(message);
    this.name = 'IpcError';
    this.command = command;
    this.code = code;
  }
}

/**
 * 信封判定：Rust `ApiResponse.success` 是裸 bool（**无** `skip_serializing_if`），故 95/95 命令的返回里
 * `success` 恒存在 → 「是对象且有 boolean `success` 字段」即信封，判定可靠。
 * 非信封值（非 Tauri mock 的 undefined、将来可能出现的裸返回）原样透传，不误伤。
 */
function isEnvelope(v: unknown): v is ApiResponse<unknown> {
  return (
    typeof v === 'object' &&
    v !== null &&
    typeof (v as { success?: unknown }).success === 'boolean'
  );
}

/**
 * 拆信封（全链路唯一拆包点）。
 *  - `success:true` → 返回 `data`。**`data` 缺省是合法成功态**：33/95 命令（proxy_start / config_save /
 *    renderer_ready 等 void 语义）走 Rust `ok_void()` → `{ success:true }`（`data` 被
 *    `skip_serializing_if` 略去），对应 api-client 的 `Promise<void>`，此处返 undefined，**不得抛错**。
 *  - `success:false` → 抛 `IpcError`（带 error/code），让调用方 catch 到真实错误，而不是把
 *    `{success:false}` 当数据用。
 */
function unwrap<T>(cmd: string, raw: unknown): T {
  if (!isEnvelope(raw)) return raw as T;
  if (raw.success) return raw.data as T;
  const message = raw.error ?? i18n.t('errors.ipcMissingMessage', { command: cmd });
  // 与 transport 失败同等可见：调用方大量 `.catch(() => {})`，不记日志就会静默吞掉后端业务失败。
  console.error(`[ipc] invoke("${cmd}") returned failure envelope:`, message, raw.code ?? '');
  throw new IpcError(cmd, message, raw.code);
}

/**
 * 调用后端命令。
 * @param cmd 命令名（对齐 Rust `#[tauri::command]` 函数名，Polaris 约定用 Polaris IPC_CHANNELS 串）
 * @param args 参数（Tauri 会序列化为 JSON 传给 Rust；裸标量需包成对象，见 invokeScalar）
 * @returns 后端 `ApiResponse` 信封里的 `data`（已拆包）；void 语义的命令返 undefined
 * @throws {IpcError} 后端返回 `success:false` 时（`message`=信封 error，`code`=信封 code）
 */
export async function invoke<T = unknown>(cmd: string, args?: unknown): Promise<T> {
  if (!isTauri()) {
    console.warn(`[ipc] invoke("${cmd}") called outside Tauri — returning mock.`);
    return undefined as unknown as T;
  }
  // Tauri invoke 签名：invoke<T>(cmd: string, args?: InvokeArgs) => Promise<T>
  // args 为 Record<string, unknown> | undefined。裸标量须包 { value }（见 invokeScalar）。
  let raw: unknown;
  try {
    raw = await tauriInvoke<unknown>(cmd, (args ?? {}) as Record<string, unknown>);
  } catch (err) {
    // 在**收口处**记一次日志再原样抛出：调用方大量存在 `.catch(() => {})`（非 Tauri 环境的历史兜底），
    // 会把真实失败吞成静默 undefined —— 命令改名/未注册这类 `Command xxx not found` 曾因此全程无声。
    // 此处只记录不改语义（仍 reject，调用方 catch 行为不变），保证任何 invoke 失败至少在控制台可见。
    console.error(`[ipc] invoke("${cmd}") failed:`, err);
    throw err;
  }
  // 拆信封刻意放 try 外：transport / 原生错误（命令未注册、序列化失败、无 Tauri runtime）保持原样上抛，
  // 不被信封逻辑吞掉或改写成 IpcError。
  return unwrap<T>(cmd, raw);
}

/**
 * 调用期望「裸标量参数」的命令（Polaris 不少 IPC 通道直接传 string/boolean，如 setPrivacyMode(value)）。
 * Tauri invoke 要求 args 是对象，故包成 { value }（Rust 侧命令签名取 `value: T`）。
 * 仅 api-client 内部用，前端调用方仍传裸标量（签名兼容）。
 */
export async function invokeScalar<T = unknown>(
  cmd: string,
  scalar: unknown
): Promise<T> {
  return invoke<T>(cmd, { value: scalar });
}

export type EventHandler<T> = (payload: T) => void;

/**
 * 订阅后端事件。同步返回退订闭包（与 Polaris ipcClient.on 签名一致），内部异步追真 unlisten。
 *
 * 注意：Tauri listen 是异步的（返回 Promise<UnlistenFn>）。在 resolve 前 cleanup 被调时，
 * 用 pending 标记推迟：unlisten 到手后若已 cleaned 则立即退订。
 *
 * @param event 事件名（对齐 Rust `app_handle.emit(name, payload)` 的 name）
 * @param handler 收到事件时回调，payload 已从 Tauri Event.payload 提取
 * @returns 退订函数（调用方 useEffect cleanup 必须调用）
 */
export function listen<T = unknown>(event: string, handler: EventHandler<T>): () => void {
  if (!isTauri()) {
    console.warn(`[ipc] listen("${event}") called outside Tauri — no-op subscription.`);
    return () => {};
  }

  let unlisten: UnlistenFn | null = null;
  let cleaned = false;

  // 不 await：同步返回退订闭包。错误（事件系统故障）仅日志，不拖垮渲染端。
  tauriListen<T>(event, (e) => {
    handler(e.payload);
  })
    .then((fn) => {
      unlisten = fn;
      // await 在途时已被 cleanup → 立即退订防泄漏
      if (cleaned) {
        fn();
        unlisten = null;
      }
    })
    .catch((err) => {
      console.error(`[ipc] listen("${event}") failed to register:`, err);
    });

  return () => {
    cleaned = true;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
    // 若 unlisten 尚未 resolve（pending），then 分支会看到 cleaned=true 并立即退订。
  };
}

/**
 * 等到 Tauri 事件监听真正登记完成后再返回退订函数。
 *
 * 普通页面可继续使用同步外观的 [`listen`]；需要“监听已就绪 → 取初始快照”的数据面使用本入口，
 * 否则 `listen()` 内部 Promise 尚未 resolve 时生产端已推进游标，会在登记窗口里丢掉首个变化。
 */
export async function listenReady<T = unknown>(
  event: string,
  handler: EventHandler<T>
): Promise<() => void> {
  if (!isTauri()) {
    console.warn(`[ipc] listenReady("${event}") called outside Tauri — no-op subscription.`);
    return () => {};
  }
  try {
    return await tauriListen<T>(event, (e) => handler(e.payload));
  } catch (err) {
    console.error(`[ipc] listenReady("${event}") failed to register:`, err);
    throw err;
  }
}

/**
 * IPC 客户端类（保留 Polaris 的 IpcClient 形态，供渐进迁移期老代码 `new IpcClient()` 或 `ipcClient.on` 用）。
 * 内部全部转发到上面的函数式 API。
 */
export class IpcClient {
  async invoke<T = unknown>(cmd: string, args?: unknown): Promise<T> {
    return invoke<T>(cmd, args);
  }

  on<T = unknown>(event: string, listener: EventHandler<T>): () => void {
    return listen<T>(event, listener);
  }

  once<T = unknown>(event: string, listener: EventHandler<T>): void {
    // Tauri 无原生 once；listen 后首次回调即退订
    const off = listen<T>(event, (payload) => {
      off();
      listener(payload);
    });
  }

  // Polaris 兼容：off/removeAllListeners 在 Tauri 模型下退订由各 listen 返回的闭包自治，
  // 这里保留空方法供老调用路径不报错。
  off(): void {
    /* no-op：退订由 listen 返回的闭包自治 */
  }
  removeAllListeners(): void {
    /* no-op */
  }
}

/** 全局 IPC 客户端实例（Polaris 兼容导出）。 */
export const ipcClient = new IpcClient();

/** 便捷函数：取消监听（Polaris 兼容导出；Tauri 模型下退订由闭包自治，此为 no-op）。 */
export function off(): void {
  /* no-op */
}

export function once<T = unknown>(event: string, listener: EventHandler<T>): void {
  ipcClient.once<T>(event, listener);
}
