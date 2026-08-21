/**
 * 托盘浮层文案（**五语种**：zh-CN / zh-TW / en-US / ru / fa）。
 *
 * # 这里曾经是什么（被修的缺陷）
 *
 * 原实现是 `t(zh: string, en: string)` —— 位置参数的双语字面量，只有「中/英」二态：zh-TW 归中、
 * **ru / fa 用户在托盘看到英文**。产品出 5 个语种，托盘却整个漏在 i18n 体系外。
 * 当时的取舍写的是「避免把整套 i18next（+5 份 locale）塞进这个小浮层」——那个顾虑是真的
 * （5 份 locale 压成 JS 对象 537 kB），但结论下错了：不必二选一。见 `i18n/auxiliary.ts` 的命名空间
 * 具名导入 —— 同一批 locale 文件、只付 `tray.*` 这一棵子树的体积（实测 3.2 kB）。
 *
 * # 语言真值源（未变）
 *
 * `localStorage['polaris.language']` + `navigator.languages`，与主窗首屏解析同源；浮层与主窗同为
 * `WebviewUrl::App` ⇒ 同源、localStorage 天然共享。主窗改语言 → 写该键 → 后端 config 也变 →
 * 广播 `configChanged` → 浮层这里重解析（`refreshTrayLang`，见 `TrayMenu.tsx` 的两个调用时刻）。
 */

import { createAuxI18n } from '@/i18n/auxiliary';
import { tray as zhCN } from '@/i18n/locales/auxiliary/zh-CN.json';
import { tray as zhTW } from '@/i18n/locales/auxiliary/zh-TW.json';
import { tray as enUS } from '@/i18n/locales/auxiliary/en-US.json';
import { tray as ru } from '@/i18n/locales/auxiliary/ru.json';
import { tray as fa } from '@/i18n/locales/auxiliary/fa.json';

/**
 * 浮层可用的全部文案键（`tray.` + en-US 那棵子树的键名）。
 *
 * **迁移完整性就靠它**：`t()` 只收 `TrayKey` 字面量联合（可选插值 vars，见 `t('tray.actionFailed', {…})`）
 * ⇒ 任何残留的 `t('中文', 'English')` 是类型不符，`tsc --noEmit` 必然报错。
 * 漏改一处 = 编译不过，不依赖肉眼核对。
 */
export type TrayKey = `tray.${Extract<keyof typeof enUS, string>}`;

const i18n = createAuxI18n<TrayKey>('tray', {
  'zh-CN': zhCN,
  'zh-TW': zhTW,
  'en-US': enUS,
  ru,
  fa,
});

/** 当前浮层语言（供组件作为渲染依赖）。 */
export const trayLang = i18n.lang;

/**
 * 重解析并返回当前语言（`configChanged` / 浮层获焦时调）。
 *
 * warm 开启时浮层 webview 会在启动后后台预建并长期保温；关闭时按需创建、隐藏 120s 后回收。模块级常量在
 * **这一代 WebView**内不会自行重算。没有这条重解析腿，用户在主窗改语言后只能等回收重建，当前浮层
 * 会继续显示旧语言。
 */
export const refreshTrayLang = i18n.refresh;

/** 取浮层文案。键即 `locales/*.json` 的 `tray.*`，5 语种齐备（缺哪个语种由 locale-parity 门先转红）。 */
export const t = i18n.t;
