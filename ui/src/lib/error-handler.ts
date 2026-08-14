/**
 * 错误处理（Polaris 底座移植自 Polaris lib/error-handler.ts）。
 *
 * toast 门面由 App 注入真实实现，启动早期回落 console；错误分类只认后端结构化 ProxyErrorCode，
 * 不再按某一种语言的错误句子猜测类别。
 */

import {
  ProxyErrorCode,
  isProxyErrorCode,
} from '../contracts/types';

export enum ErrorCategory {
  Config = 'Config',
  Connection = 'Connection',
  System = 'System',
  Process = 'Process',
  Unknown = 'Unknown',
}

/**
 * 每条 toast 的可选行为（缺省 = 原型的一次性通知：独立成条、2.2s 后自散）。
 *
 * 语义对齐 sonner（上游 用的那个库）的 `toast.x(msg, { id, duration })`：`key` ≙ sonner 的 `id`，
 * `sticky` ≙ `duration: Infinity`。名字不叫 `id` 是为了不与 Toaster 内部的自增流水号撞名。
 */
export interface ToastOptions {
  /**
   * 去重键：同 key 的后续调用**更新那一条**，不新增。
   *
   * 没有它，任何「同一件事的持续进展」都会按事件条数刷屏——一轮 50 个节点的测速会推 50 条 toast。
   * 队列语义见 `components/layout/toast-queue.ts`。
   */
  key?: string;
  /**
   * 不自动消失（须由同 key 的后续调用顶掉）。**只给「持续状态」用**，一次性通知不许开
   * ——判据（反馈存活时长 = 所陈述事实的有效期）见 `toast-queue.ts` 文件头第三节。
   */
  sticky?: boolean;
  /** 第二段小字（`.toast-desc`）。`error` 的第二位参数是它的同义简写，两者择一即可。 */
  description?: string;
  /**
   * 行内动作组。`label` 必须是已翻译的字面；有动作时 toast 会获得较长但有限的停留时间。
   * 当前消费者是测速中断态的「继续剩余 / 重新测速」。
   */
  actions?: Array<{ label: string; onClick: () => void }>;
  /** 可关闭入口；存在即渲染关闭按钮，`label` 作为已翻译的无障碍名称。 */
  dismiss?: { label: string };
}

/**
 * Toast 桥（底座占位）：Aurora 设计系统接入后由 App 注入真实 toast 实现。
 * 底座阶段默认 console 输出，保证 error-handler 可独立工作、不阻塞 tsc/打包。
 */
export type ToastImpl = {
  success: (msg: string, opts?: ToastOptions) => void;
  info: (msg: string, opts?: ToastOptions) => void;
  warning: (msg: string, opts?: ToastOptions) => void;
  error: (msg: string, description?: string, opts?: ToastOptions) => void;
};

const consoleToast: ToastImpl = {
  success: (m) => console.info(`[toast.success] ${m}`),
  info: (m) => console.info(`[toast.info] ${m}`),
  warning: (m) => console.warn(`[toast.warning] ${m}`),
  error: (m, d) => console.error(`[toast.error] ${m}`, d ?? ''),
};

let toastImpl: ToastImpl = consoleToast;

/** 注入真实 toast 实现（App 挂载时调，接入 Aurora 设计系统的 Toaster）。 */
export function setToastImpl(impl: Partial<ToastImpl>): void {
  toastImpl = { ...consoleToast, ...impl };
}

/** toast 门面：转发到当前注入的实现，故消费方无需感知注入时机（未注入时落 console）。 */
export const toast: ToastImpl = {
  success: (m, o) => toastImpl.success(m, o),
  info: (m, o) => toastImpl.info(m, o),
  warning: (m, o) => toastImpl.warning(m, o),
  error: (m, d, o) => toastImpl.error(m, d, o),
};

/**
 * F15：代理错误码 → ErrorCategory 映射（跨进程错误分类的唯一依据）。
 * 非法/未知码返回 null；调用方应按未知错误处理，不按本地化文案反推类别。
 */
export function proxyErrorCategory(code: unknown): ErrorCategory | null {
  if (!isProxyErrorCode(code)) return null;
  switch (code) {
    case ProxyErrorCode.DEST_CONNECTION_REFUSED:
    case ProxyErrorCode.CONNECTION_REFUSED:
    case ProxyErrorCode.CONNECTION_TIMEOUT:
    case ProxyErrorCode.DNS_RESOLVE_FAILED:
    case ProxyErrorCode.TLS_CERT_ERROR:
    case ProxyErrorCode.AUTH_FAILED:
      return ErrorCategory.Connection;
    case ProxyErrorCode.CONFIG_INVALID:
    case ProxyErrorCode.PORT_IN_USE:
    case ProxyErrorCode.CLASH_API_PORT_RECYCLING:
      return ErrorCategory.Config;
    case ProxyErrorCode.PERMISSION_DENIED:
    case ProxyErrorCode.SYSTEM_PROXY_FAILED:
    case ProxyErrorCode.EXIT_MISMATCH:
    case ProxyErrorCode.RULE_RESOURCES_MISSING:
    case ProxyErrorCode.BINARY_NOT_EXECUTABLE:
    case ProxyErrorCode.BINARY_NOT_FOUND:
    case ProxyErrorCode.CRONET_LIB_MISSING:
    case ProxyErrorCode.HELPER_NOT_INSTALLED:
    case ProxyErrorCode.HELPER_GATE_ABORTED:
    case ProxyErrorCode.TUN_ROUTE_NOT_CAPTURED:
      return ErrorCategory.System;
    case ProxyErrorCode.STARTUP_FAILED:
    case ProxyErrorCode.PROCESS_KILLED:
    case ProxyErrorCode.PROCESS_EXITED:
    case ProxyErrorCode.AUTO_RESTARTING:
    case ProxyErrorCode.AUTO_RESTART_FAILED:
    case ProxyErrorCode.RESTART_LIMIT_REACHED:
    case ProxyErrorCode.STOP_AUTH_CANCELLED:
    case ProxyErrorCode.CORE_UPDATE_IN_PROGRESS:
      return ErrorCategory.Process;
    default:
      return null; // UNKNOWN
  }
}
