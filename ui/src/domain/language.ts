/**
 * 界面语言：受支持语言集 + 「自动（跟随系统）」解析 + 旧码迁移。纯函数，主/渲染/单测共用，零副作用。
 *
 * 关键：i18n 资源键用 `fa`（非 `fa-IR`）——与其余「只在消歧时带地区」的口径一致（ru 裸码、zh-CN/zh-TW 必须区分简繁）；
 * 也与 Chromium/Electron 的 `fa.pak`（无 `fa-IR.pak`）对齐。旧版本存的 `fa-IR` 经 migrateLanguageCode 迁移。
 */

/** 受支持的界面语言（i18n 资源键）。顺序无语义（UI 自行排序）。 */
export const SUPPORTED_LANGUAGES = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'] as const;
export type SupportedLanguage = (typeof SUPPORTED_LANGUAGES)[number];

/** 无匹配时的回退语言（国际通用，亦为 i18n fallbackLng）。 */
export const DEFAULT_LANGUAGE: SupportedLanguage = 'en-US';

/** 语言选择的「自动（跟随系统）」哨兵值（真值源 = config.language；旧版存 localStorage['app-language']）。 */
export const AUTO_LANGUAGE = 'auto';

/** 语言选择的 localStorage 键（首屏同步解析用的本地缓存；真值源仍是 `config.language`）。 */
export const LANGUAGE_STORAGE_KEY = 'polaris.language';

// RTL 语言集合：以语言前缀匹配（fa / fa-IR / ar / he / ur / ...），新增 RTL 语言时在此追加前缀。
const RTL_LANGUAGE_PREFIXES = ['fa', 'ar', 'he', 'ur'];

/**
 * 判断某语言代码是否为 RTL（按前缀，忽略地区子标签）。
 *
 * 放在 domain 层而非 `i18n/index.ts`：它是**纯语言谓词**、零依赖，而 `i18n/index.ts` 一进来就带
 * i18next + 5 份 locale 全量（537 kB）。托盘浮层与更新弹窗两个辅助 webview 也要判 RTL，从这里取
 * 才不会为了一个 4 元素前缀表把整个 i18next 拖进 3 kB 的弹窗包。
 */
export function isRtlLanguage(lng: string): boolean {
  const primary = (lng || '').toLowerCase().split('-')[0];
  return RTL_LANGUAGE_PREFIXES.includes(primary);
}

/** 旧语言码迁移：`fa-IR` → `fa`（其余原样返回，含 null/undefined 透传）。 */
export function migrateLanguageCode(code: string | null | undefined): string | null | undefined {
  return code === 'fa-IR' ? 'fa' : code;
}

/** 单个 BCP47 语言码 → 受支持语言；无匹配返 null。按主语言子标签 + 脚本/地区消歧。 */
function matchSupported(raw: string): SupportedLanguage | null {
  const l = (raw || '').toLowerCase();
  if (!l) return null;
  const primary = l.split(/[-_]/)[0];
  if (primary === 'zh') {
    // 繁体：Hant 脚本 或 台/港/澳 地区；其余（Hans/cn/sg/裸 zh）→ 简体。
    if (l.includes('hant') || /(^|[-_])(tw|hk|mo)([-_]|$)/.test(l)) return 'zh-TW';
    return 'zh-CN';
  }
  if (primary === 'fa') return 'fa';
  if (primary === 'ru') return 'ru';
  if (primary === 'en') return 'en-US';
  return null;
}

/**
 * OS 偏好语言有序列表（app.getPreferredSystemLanguages，如 ['zh-Hans-CN','en-US']）→ 受支持语言。
 * 逐个匹配、命中即止；全不匹配 → DEFAULT_LANGUAGE（en-US）。
 */
export function resolveAutoLanguage(
  preferred: readonly string[] | null | undefined
): SupportedLanguage {
  for (const raw of preferred || []) {
    const m = matchSupported(raw);
    if (m) return m;
  }
  return DEFAULT_LANGUAGE;
}

/**
 * 解析有效界面语言：
 * - choice='auto' / 未设 / 旧 fa-IR 之外的非法值 → 按系统偏好解析；
 * - choice 是受支持的具体码（fa-IR 先迁移成 fa）→ 用它。
 */
export function resolveEffectiveLanguage(
  choice: string | null | undefined,
  systemLanguages: readonly string[] | null | undefined
): SupportedLanguage {
  const c = migrateLanguageCode(choice);
  if (!c || c === AUTO_LANGUAGE) return resolveAutoLanguage(systemLanguages);
  return (SUPPORTED_LANGUAGES as readonly string[]).includes(c)
    ? (c as SupportedLanguage)
    : resolveAutoLanguage(systemLanguages);
}
