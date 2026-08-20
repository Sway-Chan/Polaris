/**
 * 托盘浮层主题折算：把 config.uiTheme（'light' | 'dark' | 'system'/未设）折成 <html data-theme>。
 *
 * 抽成共享模块的原因：首帧（main.tsx）、hydrate、系统主题变化监听、窗口 focus 四处都要用**同一口径**
 * 折算，各写一份易漂移。此前正是分裂的：main.tsx 首帧只认 matchMedia、hydrate 认 config.uiTheme——用户
 * 切了系统主题后再点托盘，保温期内复用的浮层 DOM 还挂着上次的 data-theme，show 首帧就「闪一下旧主题」。
 * tokens 默认深色（无显式/系统信号时偏深）。
 */

/** uiTheme → 是否深色。显式 light/dark 直接定；其余（'system'/未设）跟随系统 prefers-color-scheme。 */
function resolveDark(uiTheme?: 'light' | 'dark' | 'system' | null): boolean {
  if (uiTheme === 'dark') return true;
  if (uiTheme === 'light') return false;
  return window.matchMedia('(prefers-color-scheme: dark)').matches;
}

/**
 * 折算并**同步**写到 <html data-theme>。同步是关键：focus/show 时先同步校正、再异步 hydrate 拉真值，
 * 杜绝「先渲染旧主题再纠正」的闪烁。
 */
export function applyTrayTheme(uiTheme?: 'light' | 'dark' | 'system' | null): void {
  document.documentElement.setAttribute('data-theme', resolveDark(uiTheme) ? 'dark' : 'light');
}
