/**
 * 主题折算（`<html data-theme>` 的运行期真值）—— 纯函数，AppShell 的主题 effect 只负责接线。
 *
 * # 为什么必须有「未水合」这一档（本模块存在的唯一理由）
 *
 * 主进程 `tray::theme_boot_script` 在**第一帧之前**按 `config.uiTheme` 真值播种了 `data-theme`
 * （它是唯一能同步读到 uiTheme 的地方——那个值在 config.json 里，前端拿到它已经是 IPC 之后）。
 * 若前端挂载时不加区分地按 `config?.uiTheme ?? 'system'` 重写这个属性，就等于**在 config 水合前
 * 把种子覆写成「跟随系统」**：`uiTheme=dark` + OS 浅色的用户冷启动会看到
 * 「首帧深（种子）→ React 挂载闪浅 → config 到达转回深」——FOUC 没被修掉，只是从「首帧前」挪到了
 * 「首帧后」。2026-07-28 独立复审抓出（种子脚本写的 `window.__POLARIS_INITIAL_THEME__` 当时全仓零消费，
 * 正是这条腿缺失的化石证据）。
 *
 * 故未水合时的回落值必须是**种子**，不是 `'system'`：种子已经是按用户真实选择算出来的结论。
 * 拿不到种子（注入失败 / 浏览器里直接开 dist / 单测）才回落系统偏好——那与 `index.html` 里
 * `html:not([data-theme])` 的 CSS 兜底同口径，不制造新的观感跳变。
 */

/** `<html data-theme>` 的值域。 */
export type ThemeAttr = 'dark' | 'light';

/** 主进程种子脚本注入的全局（`tray::theme_boot_script`）。缺失 = 非 Tauri / 注入失败。 */
declare global {
  interface Window {
    /** 建窗那一刻按 `config.uiTheme` 折算出的主题（`'dark'` / `'light'`）。 */
    __POLARIS_INITIAL_THEME__?: string;
  }
}

export interface ThemeInput {
  /**
   * `config.uiTheme`。**`undefined` 专指「config 尚未水合」**——不是「配置里没写这个字段」：
   * 后者由调用方折成 `'system'`（store 里 config 非 null 即已水合，见 AppShell 的 configLoaded）。
   */
  uiTheme: 'light' | 'dark' | 'system' | undefined;
  /** `window.__POLARIS_INITIAL_THEME__`（主进程种子）。非 'dark'/'light' 的值一律当缺失。 */
  seed: string | undefined;
  /** `matchMedia('(prefers-color-scheme: dark)').matches`。 */
  systemDark: boolean;
}

/** 种子归一：只认 `'dark'`/`'light'`，其余（含 undefined / 被篡改的值）当没有。 */
function normalizeSeed(seed: string | undefined): ThemeAttr | undefined {
  return seed === 'dark' || seed === 'light' ? seed : undefined;
}

/**
 * 折出该写进 `<html data-theme>` 的值。
 *
 * 优先级：
 *  1. **config 已水合** → 它是运行期唯一真值（显式 light/dark 直接定，`'system'` 跟随系统）。
 *     种子在这一档**不参与**：用户在设置里改主题即刻生效走的就是这条腿，种子是建窗那一刻的旧快照。
 *  2. **config 未水合 + 有种子** → 用种子（= 主进程按 uiTheme 真值算出来的同一结论）⇒ 与首帧一致，无闪。
 *  3. **config 未水合 + 无种子** → 跟随系统（与 index.html 的 CSS 兜底同口径）。
 */
export function resolveTheme({ uiTheme, seed, systemDark }: ThemeInput): ThemeAttr {
  if (uiTheme === 'dark') return 'dark';
  if (uiTheme === 'light') return 'light';
  if (uiTheme === undefined) {
    const s = normalizeSeed(seed);
    if (s !== undefined) return s;
  }
  return systemDark ? 'dark' : 'light';
}

/** 读主进程种子（非浏览器环境返 undefined）。 */
export function readInitialThemeSeed(): string | undefined {
  return typeof window === 'undefined' ? undefined : window.__POLARIS_INITIAL_THEME__;
}
