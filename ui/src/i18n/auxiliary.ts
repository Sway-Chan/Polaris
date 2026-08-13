/**
 * 辅助 webview 的极简 i18n（托盘浮层 `tray/` + 更新弹窗 `update-popup/`）。
 *
 * # 为什么不是「直接复用主窗 i18next」
 *
 * 两个辅助窗是**独立入口、独立 JS 堆**（`vite.config.ts` 的多入口；`tray.html` / `update-popup.html`
 * 各自建窗，见 `tray.rs::build_overlay` 与 `runtime/update_popup.rs`）。`import i18n from '@/i18n'`
 * 会把 i18next + react-i18next + **5 份 locale 全量**拖进来 —— 实测 5 份 locale 压成 JS 对象
 * 是 **537 kB**（gzip 231 kB）。更新弹窗当前整包 3.1 kB，那是 170 倍；且 react-i18next 还会把
 * React 拖进这个**零框架 vanilla TS** 的窗。
 *
 * # 选的路：同一批 locale 文件，按命名空间做**具名导入**，Rollup 摇掉其余
 *
 * Vite 的 JSON 插件默认 `namedExports: true` ⇒ `import { tray } from './locales/en-US.json'`
 * 只保留 `tray` 这一棵子树，其余顶层命名空间被 tree-shake 掉（实测：5 份 locale 具名导入单个
 * 命名空间，产物 **3.2 kB**）。于是：
 *  - **真值源仍是那 5 份 locale**（翻译者只看一处；`locale-parity.test.ts` 的键集/形态/棘轮门原样覆盖）；
 *  - 辅助窗只付它自己那点文案的体积；
 *  - 零新增依赖、零构建腿（不生成子集文件，故没有「生成物过期」这类新失效模式）。
 *
 * # 语言从哪来（**不依赖主窗**）
 *
 * 读 `localStorage['polaris.language']` + `navigator.languages`，走 `domain/language` 的同一套解析
 * （`migrateLanguageCode` → `resolveEffectiveLanguage`）。这与主窗 `i18n/index.ts` 的**首屏**解析
 * 完全同源，且同源 webview（两个窗都是 `WebviewUrl::App`）天然共享 localStorage。
 *
 * 关键时序：更新弹窗可能在主窗尚未初始化（甚至不存在）时弹出 —— 本模块**同步、零 IPC、零 await**，
 * 不假设任何主窗状态就绪。这与弹窗「首帧不依赖 IPC」的既有不变式一致。
 *
 * # 缺键怎么办
 *
 * 当前语种缺 → 回落 `en-US`；en-US 也缺 → **返回键名本身**（显式坏相，不静默显示别的语言）。
 * 后者不该发生：`locale-parity.test.ts` 的键集门 + `i18n-coverage.test.ts` 的辅助窗键门都会先转红。
 */

import {
  AUTO_LANGUAGE,
  DEFAULT_LANGUAGE,
  isRtlLanguage,
  LANGUAGE_STORAGE_KEY,
  migrateLanguageCode,
  resolveEffectiveLanguage,
  type SupportedLanguage,
} from '../domain/language';

/** 一个命名空间的扁平文案表（`locales/*.json` 里 `tray` / `updatePopup` 那棵子树）。 */
export type AuxBundle = Readonly<Record<string, string>>;

/**
 * 解析辅助窗当前应显示的语言。**每次调用都重读 localStorage**。
 *
 * 浮层/弹窗 webview 建成后常驻不重载（隐藏≠销毁），模块级 `const` 求值一次就会被钉死在启动时的语言上
 * ——用户在主窗改语言后浮层再也跟不上，必须重启 App。故解析必须是函数，且在
 * 「`configChanged` / 窗口获焦」等时刻重新调用（见 `TrayMenu.tsx`）。
 */
export function resolveAuxLanguage(): SupportedLanguage {
  let choice = '';
  try {
    choice = localStorage.getItem(LANGUAGE_STORAGE_KEY) ?? '';
  } catch {
    /* 受限上下文禁 localStorage（沙箱/隐私）：回落系统偏好，不抛 */
  }
  const langs =
    typeof navigator !== 'undefined' && Array.isArray(navigator.languages) ? navigator.languages : [];
  return resolveEffectiveLanguage(migrateLanguageCode(choice) || AUTO_LANGUAGE, langs);
}

/** 插值：`{{name}}` → `vars.name`（与 i18next 同语法，翻译者只需记一套）。缺变量原样保留，便于暴露。 */
function interpolate(s: string, vars?: Readonly<Record<string, string | number>>): string {
  if (!vars) return s;
  return s.replace(/\{\{(\w+)\}\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m));
}

export interface AuxI18n<K extends string> {
  /** 取文案。`key` 是**带命名空间前缀的全键**（`tray.connect`），与主窗 `t('a.b.c')` 同形。 */
  t(key: K, vars?: Readonly<Record<string, string | number>>): string;
  /** 当前语言码。 */
  lang(): SupportedLanguage;
  /** 重解析语言并同步 `<html lang/dir>`，返回新语言码。 */
  refresh(): SupportedLanguage;
}

/**
 * 建一个命名空间作用域的 aux i18n。
 *
 * @param ns       命名空间名（`'tray'` / `'updatePopup'`），= locale JSON 的顶层键，也是 `t()` 键的前缀。
 * @param bundles  五语种的该命名空间子树（由调用方具名导入，Rollup 据此摇掉其余命名空间）。
 *
 * 调用方把 `K` 绑成 `` `${ns}.${keyof typeof enBundle}` ``，于是**任何拼错/漏改的键都是编译错误**
 * ——这正是把 `t(zh, en)` 迁到键查找时「一个调用点都不能漏」的保证方式（`tsc --noEmit` 即证明）。
 */
export function createAuxI18n<K extends string>(
  ns: string,
  bundles: Readonly<Record<SupportedLanguage, AuxBundle>>
): AuxI18n<K> {
  let lang = resolveAuxLanguage();
  const prefix = ns + '.';

  const apply = (lng: SupportedLanguage): void => {
    // 辅助窗也要跟 RTL：fa 用户的浮层/弹窗必须整体镜像，否则图标与文字左右错位。
    document.documentElement.dir = isRtlLanguage(lng) ? 'rtl' : 'ltr';
    document.documentElement.lang = lng;
  };
  apply(lang);

  return {
    t(key, vars) {
      // 去掉命名空间前缀取表内键；调用点写全键是为了让 `locale-parity.test.ts` 的
      // `t('a.b.c')` 扫描器**看得见**这些消费点（否则辅助窗又会整个漏在可寻址性门外）。
      const k = key.startsWith(prefix) ? key.slice(prefix.length) : key;
      const hit = bundles[lang][k] ?? bundles[DEFAULT_LANGUAGE][k];
      return interpolate(hit ?? key, vars);
    },
    lang: () => lang,
    refresh() {
      lang = resolveAuxLanguage();
      apply(lang);
      return lang;
    },
  };
}
