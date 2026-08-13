/**
 * 应用分流屏的排序判据（纯函数，脱离组件可断言 —— 本仓 vitest 走 node 环境无 jsdom，
 * 判据留在 .tsx 里就等于没有门）。
 *
 * 为什么必须带 locale：`a.localeCompare(b)` 不传 locale 时用**运行时默认语言环境**（本机 node 与
 * 多数 webview 解析为 en-US），而 en-US 的排序对汉字退化成码位序 —— 「工具/游戏/社交/视频」会排成
 * 工具·游戏·社交·视频，既不是拼音序也不是笔画序，用户读作「没排序」。传 zh-CN 才走 CLDR 拼音
 * 整理（工具·社交·视频·游戏）。裸 `<` 更差（纯码位）。
 *
 * 副作用：zh 的 CLDR 整理含 `[reorder Hani]`（汉字排在拉丁之前），故中文界面下「AI」这类拉丁名
 * 类目恒落在汉字类目之后。这是标准中文整理的既定语义，不另做特例覆盖。
 */

/** 「全部」不是类目而是「不过滤」，排序时恒置首（拼音序下「全部」会掉进 工具/社交 之间，语义错位）。 */
export const APP_CATEGORY_ALL = 'all';

export interface LabeledCategory {
  key: string;
  label: string;
}

/**
 * 显示名比较判据 —— 分类筛选下拉、内容分组、组内应用三处共用同一份，避免各排各的。
 * locale 传当前界面语言（i18n.language，恒为 SupportedLanguage 之一，故不会是非法 BCP-47 标签）。
 */
export function compareLabel(a: string, b: string, locale: string): number {
  return a.localeCompare(b, locale);
}

/**
 * 分类排序：'all' 恒首，其余按当前语言下的显示名。
 *
 * 返回新数组（`Array.prototype.sort` 原地改），调用方传的是 memo 依赖里的常量表，不能被就地打乱。
 */
export function sortAppCategories<T extends LabeledCategory>(
  cats: readonly T[],
  locale: string,
): T[] {
  return [...cats].sort((a, b) => {
    if (a.key === APP_CATEGORY_ALL || b.key === APP_CATEGORY_ALL) {
      // 两边都是 'all' 时返回 0（sort 稳定 → 保持原相对次序），否则 'all' 那侧在前。
      return (a.key === APP_CATEGORY_ALL ? 0 : 1) - (b.key === APP_CATEGORY_ALL ? 0 : 1);
    }
    return compareLabel(a.label, b.label, locale);
  });
}
