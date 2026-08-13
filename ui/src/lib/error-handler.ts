/**
 * 错误处理（Polaris 底座移植自 Polaris lib/error-handler.ts）。
 *
 * Polaris 原用 sonner（toast）做用户可见反馈。Polaris 底座阶段未引入 toast 库（待组件阶段随设计系统接入），
 * 故此处 showError/showSuccess/showInfo/showWarning 暂走 console，待 Aurora 设计系统接入后替换为真实 toast。
 * 错误分类（ErrorCategory）、proxyErrorCategory 映射与 Polaris 完全一致——这是跨进程错误分类的唯一依据，
 * 底座即需就位（后端 Rust 错误码 → 前端分类）。
 */

import i18n from '../i18n';
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

export interface AppError {
  category: ErrorCategory;
  userMessage: string;
  technicalMessage?: string;
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
   * 行内动作按钮（当前唯一消费者：测速中断态的「继续」）。
   *
   * ⚠️ **带 action 的 toast 一定不是 sticky**：`toast-queue.ts::autoDismissMs` 让 action **压过**
   * `sticky`，返回一个更长但**有限**的停留（`ACTION_VISIBLE_MS`）。判据：一条按钮点不到又关不掉的
   * toast 会永久占着屏幕右下角，比没有按钮更糟；而 2.2s 的默认停留又短到按钮形同虚设。
   * 两难只有一个出口 —— 停留加长但必须**收敛**。这条不变式由 toast-queue 的门钉死。
   *
   * `label` 必须是**已翻译**的字面（本层不碰 i18n）。`onClick` 触发后由调用方自行决定后续
   * （典型：发起续测，随后新一轮进度事件会以同 key 顶掉这条）。
   */
  action?: { label: string; onClick: () => void };
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

export class ErrorHandler {
  static handle(error: AppError): void {
    console.error(`[${error.category}] ${error.userMessage}`, error.technicalMessage);

    switch (error.category) {
      case ErrorCategory.Config:
        this.handleConfigError(error);
        break;
      case ErrorCategory.Connection:
        this.handleConnectionError(error);
        break;
      case ErrorCategory.System:
        this.handleSystemError(error);
        break;
      case ErrorCategory.Process:
        this.handleProcessError(error);
        break;
      default:
        this.handleUnknownError(error);
    }
  }

  static handleApiError(error: unknown, context: string): void {
    console.error(`API Error in ${context}:`, error);

    let userMessage = i18n.t('errors.operationFailed');
    let category = ErrorCategory.System;

    if (error instanceof Error) {
      userMessage = error.message || userMessage;
    } else if (typeof error === 'string') {
      userMessage = error;
    }

    if (this.isTrojanError(userMessage)) {
      category = ErrorCategory.Connection;
    } else if (this.isProtocolError(userMessage)) {
      category = ErrorCategory.Config;
    }

    this.handle({
      category,
      userMessage: `${context}: ${userMessage}`,
      technicalMessage: error instanceof Error ? error.stack : String(error),
    });
  }

  private static isTrojanError(message: string): boolean {
    const trojanKeywords = ['trojan', 'Trojan', '认证失败', '密码错误', 'TLS 握手失败'];
    return trojanKeywords.some((keyword) => message.includes(keyword));
  }

  private static isProtocolError(message: string): boolean {
    return (
      message.includes('不支持的协议') ||
      message.includes('Protocol') ||
      message.includes('暂不支持')
    );
  }

  static showSuccess(message: string): void {
    toastImpl.success(message);
  }

  static showInfo(message: string): void {
    toastImpl.info(message);
  }

  static showWarning(message: string): void {
    toastImpl.warning(message);
  }

  static showError(message: string, description?: string): void {
    toastImpl.error(message, description);
  }

  private static handleConfigError(error: AppError): void {
    this.showError(i18n.t('errors.configError'), error.userMessage);
  }

  private static handleConnectionError(error: AppError): void {
    this.showError(i18n.t('errors.connectionError'), error.userMessage);
  }

  private static handleSystemError(error: AppError): void {
    this.showError(i18n.t('errors.systemError'), error.userMessage);
  }

  private static handleProcessError(error: AppError): void {
    this.showError(i18n.t('errors.processError'), error.userMessage);
  }

  private static handleUnknownError(error: AppError): void {
    this.showError(i18n.t('errors.unknownError'), error.userMessage);
  }
}

/**
 * F15：代理错误码 → ErrorCategory 映射（跨进程错误分类的唯一依据）。
 * 非法/未知码返回 null，调用方回落到旧的中文字符串匹配 fallback。
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
