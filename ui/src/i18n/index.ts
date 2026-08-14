/**
 * i18n 初始化（Polaris / Tauri 2 适配）。
 *
 * Polaris 原：经 Electron webPreferences.additionalArguments 注入 OS 偏好语言 + config.language 选择，
 * 同步可读（C1 白屏防护）。Polaris 跑在 Tauri webview，无 additionalArguments 通道：
 *  - 语言选择真值源仍是后端 config.language（经 api-client 读写），但 i18n 初始化不能 await invoke
 *    （同步执行，否则首屏闪现回退语言）。故首屏用 localStorage + navigator 兜底解析，App 挂载后从
 *      config.language 校正（languageChanged 自动同步 <html lang/dir>）。
 *  - OS 偏好语言：navigator.languages（webview 原生，同 Chromium app.getPreferredSystemLanguages 语义）。
 *
 * RTL 处理、语言码迁移、isRtlLanguage 与 Polaris 完全一致（见 ../../domain/language）。
 */

import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';

import zhCN from './locales/zh-CN.json';
import zhTW from './locales/zh-TW.json';
import enUS from './locales/en-US.json';
import ru from './locales/ru.json';
import fa from './locales/fa.json';
import {
  AUTO_LANGUAGE,
  isRtlLanguage,
  LANGUAGE_STORAGE_KEY,
  migrateLanguageCode,
  resolveEffectiveLanguage,
} from '../domain/language';

const resources = {
  'zh-CN': {
    translation: zhCN,
  },
  'zh-TW': {
    translation: zhTW,
  },
  'en-US': {
    translation: enUS,
  },
  ru: {
    translation: ru,
  },
  fa: {
    translation: fa,
  },
};

/**
 * 读语言「选择」（'auto' 或具体码；空=未设）。
 * Tauri webview localStorage 在受限上下文可能被禁（沙箱/隐私）——import 期同步执行，一抛即拖垮整个
 * bundle 白屏，故 try/catch 兜 ''（同 Polaris 原写法）。
 */
function readLanguageChoice(): string {
  try {
    return localStorage.getItem(LANGUAGE_STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 持久化语言选择（App.tsx 切语言时调，经 config 同步给后端的同时写本地缓存供首屏解析）。 */
export function persistLanguageChoice(choice: string): void {
  try {
    localStorage.setItem(LANGUAGE_STORAGE_KEY, choice);
  } catch {
    /* localStorage 不可用时退化为会话内记忆，不阻断 */
  }
}

// RTL 判定已下沉 `domain/language.ts`（纯谓词、零依赖）：托盘浮层与更新弹窗两个辅助 webview 也要用它，
// 而它们不能 import 本模块（会连带拖进 i18next + 5 份 locale 全量）。此处只做**再导出**，保持单一真值源。
export { isRtlLanguage } from '../domain/language';

/** 按当前语言同步 <html dir> + <html lang>（RTL 基建唯一接线点）。 */
function applyDocumentDirection(lng: string): void {
  if (typeof document === 'undefined') return;
  document.documentElement.dir = isRtlLanguage(lng) ? 'rtl' : 'ltr';
  document.documentElement.lang = lng;
}

/**
 * 首屏语言选择：localStorage 优先（用户显式选过），否则 'auto'（跟随系统）。
 * 用 `||` 而非 `??`：拦空串（否则解析出 '' 让 App 迁移 effect 的守卫永不收敛）。
 */
export const initialLanguageChoice: string =
  migrateLanguageCode(readLanguageChoice()) || AUTO_LANGUAGE;

/** webview 原生 OS 偏好语言列表（替代 Polaris 的 additionalArguments 注入）。 */
function getSystemLanguages(): string[] {
  const langs =
    typeof navigator !== 'undefined' ? navigator.languages : undefined;
  return Array.isArray(langs) ? langs : [];
}

// 有效界面语言：'auto'/未设 → 按系统偏好解析；否则用具体码。
const effectiveLanguage = resolveEffectiveLanguage(
  initialLanguageChoice,
  getSystemLanguages()
);

void i18n.use(initReactI18next).init({
  resources,
  lng: effectiveLanguage,
  fallbackLng: 'en-US',
  interpolation: {
    escapeValue: false,
  },
});

// 同步 <html lang> + <html dir>：初始化按当前语言设置，切换语言时随 languageChanged 更新。
i18n.on('languageChanged', (lng) => {
  applyDocumentDirection(lng);
});
applyDocumentDirection(i18n.language);

/**
 * 从语言「选择」校正运行期界面语言 —— 即本模块头注承诺的「App 挂载后从 config.language 校正」那一腿。
 *
 * `choice` 取 `config.language` 的值（`'auto'` 或具体码），**它才是真值源**；localStorage 只是首屏
 * 同步解析用的本地缓存（首屏不能 await invoke，否则闪回退语言）。
 *
 * # 为什么这条腿缺了会「切语言什么都不变，连重启都不变」
 *
 * 写侧一直是通的（`SettingsDisplay.tsx` → `update({language})` → 后端 config），断的是读侧：
 * 全 `ui/src` 没有一处 `i18n.changeLanguage`，`persistLanguageChoice` 零调用者 ⇒
 * `localStorage['polaris.language']` **永远是空** ⇒ 首屏恒解析成 `'auto'` 跟随系统。所以重启也不变。
 * 同一条缺腿还打断了托盘浮层：`tray/labels.ts` 读的是同一个键，它的 `refreshTrayLang()` 重解析机制
 * 本身是好的，只是解析对象一直没被写过。补上写入即三处（主窗 / 下次首屏 / 浮层）一起收敛。
 *
 * 幂等：有效语言没变就不调 `changeLanguage`，避免受控回声触发无谓重渲染。
 * 返回生效的语言码。
 */
export function syncLanguageChoice(choice: string | null | undefined): string {
  const c = migrateLanguageCode(choice) || AUTO_LANGUAGE;
  // 先写缓存：即便有效语言没变（如 'auto' 在中文系统上解析出的仍是 zh-CN），选择本身也要落盘，
  // 否则「auto → zh-CN」这类只改选择不改结果的切换在下次冷启动会退回 auto。
  persistLanguageChoice(c);
  const eff = resolveEffectiveLanguage(c, getSystemLanguages());
  if (i18n.language !== eff) void i18n.changeLanguage(eff);
  return eff;
}

export default i18n;
