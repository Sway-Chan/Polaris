/**
 * 桌面通知（用户态系统通知）统一出口 —— C8。迁移自 上游 `src/main/notify-user.ts`（同语义）。
 *
 * 与 上游的差异：上游 在主进程用 Electron `Notification`；Polaris 在渲染端经 Tauri
 * `tauri-plugin-notification` 发。**刻意直接 invoke plugin 命令**（`plugin:notification|*`）而非引入
 * `@tauri-apps/plugin-notification` JS 包：该包只是这些 invoke 的薄封装，装它要改 package.json + lock +
 * node_modules，而 Rust 侧插件（`main.rs` 已 `.plugin(tauri_plugin_notification::init())`）与权限
 * （`capabilities/default.json` 的 `notification:default`）已就位 → 直接 invoke 零新依赖。用**裸**
 * `@tauri-apps/api/core` invoke（非 `@/ipc` 封装）：插件命令返回原始值，不是 Polaris 的 ApiResponse 信封。
 *
 * 设计（对齐 上游 notify-user）：
 * - **应用级总开关**（config.desktopNotifications，默认开）：`setDesktopNotificationsEnabled` 由 App 同步。
 * - **权限一次解析 + 缓存**：首发时查/请求系统权限；拒绝/不可用/异常一律静默降级，绝不抛、绝不阻塞调用方。
 * - **不含节点身份**：通知进系统通知中心/锁屏可见，正文只放应用自生成的通用文案/错误信息，不写节点名/域名/IP。
 */

import { invoke } from '@tauri-apps/api/core';

/** 应用级总开关（config.desktopNotifications，缺省/非 false 视为开）。 */
let enabled = true;
/** 系统权限只解析一次（首发时），之后缓存——避免每条通知都往返查权限。 */
let permissionResolved = false;
let permissionGranted = false;

/** 同步应用级总开关。启动 loadConfig + CONFIG_CHANGED 时调用（对齐 上游 setDesktopNotificationsEnabled）。 */
export function setDesktopNotificationsEnabled(value: boolean | undefined): void {
  enabled = value !== false;
}

/** 当前应用级开关（供测试/诊断）。 */
export function isDesktopNotificationsEnabled(): boolean {
  return enabled;
}

/** 测试用：重置权限缓存（每个用例独立解析）。 */
export function __resetDesktopNotifyPermissionCache(): void {
  permissionResolved = false;
  permissionGranted = false;
}

/** 首发时解析系统权限：已授予→true；未决（null）→ 请求一次；拒绝/异常→false。结果缓存。 */
async function ensurePermission(): Promise<boolean> {
  if (permissionResolved) return permissionGranted;
  try {
    // is_permission_granted 返回 Option<bool>：true=已授予 / false=已拒绝 / null=未决（Prompt）。
    let granted = await invoke<boolean | null>('plugin:notification|is_permission_granted');
    if (granted === null || granted === undefined) {
      // 未决 → 请求一次；request_permission 返回 PermissionState 串（'granted' 才算通过）。
      const state = await invoke<string>('plugin:notification|request_permission');
      granted = state === 'granted';
    }
    permissionGranted = granted === true;
  } catch {
    // 非 Tauri（浏览器/测试无 mock）/ 插件异常 → 视为无权限，静默不发。
    permissionGranted = false;
  }
  permissionResolved = true;
  return permissionGranted;
}

/**
 * 发一条桌面通知。应用级关 / 无权限 / 系统不支持 / 异常 → 静默不发，绝不抛。
 * @param title 标题（品牌/分类，语言由调用方从 i18n 取）
 * @param body  正文（应用自生成文案/错误信息，**不得含节点名等身份**）
 */
export async function notifyDesktop(title: string, body: string): Promise<void> {
  if (!enabled) return;
  if (!(await ensurePermission())) return;
  try {
    // notify 命令签名：options: NotificationData（含 title/body 等）。
    await invoke('plugin:notification|notify', { options: { title, body } });
  } catch {
    /* 通知失败不影响主流程 */
  }
}
