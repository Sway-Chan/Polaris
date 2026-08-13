/**
 * `resolveTheme` 的口径守卫 —— 守的是 2026-07-28 复审抓出的那条 FOUC 腿。
 *
 * 被守的缺陷不是「算错了」，而是「config 未水合时回落 `'system'`」：主进程
 * `tray::theme_boot_script` 已按 `config.uiTheme` 真值在第一帧之前播下 `data-theme`，前端挂载时
 * 无条件按「跟随系统」重写它，等于把正确的种子覆写成猜测 ⇒ `uiTheme=dark` + OS 浅色的用户冷启动
 * 看到「首帧深 → 挂载闪浅 → config 到达转回深」。FOUC 换了位置，没被修掉。
 *
 * 故本文件的核心用例是**「未水合 + 种子与系统偏好相反」**那一格：把回落改回 `'system'`（或把种子
 * 参数丢掉）必然转红。其余分支一并钉住，防「修这条腿时把已生效的显式 light/dark 腿弄坏」。
 *
 * 末尾一条结构断言：AppShell 必须**消费** `resolveTheme`（而不是在 effect 里内联一套三元）。
 * 种子全局在被修之前是**全仓零消费**的——零消费正是这条腿缺失的化石证据，所以「有没有人在用」
 * 本身就值得钉一道。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolveTheme } from './theme-state';

describe('resolveTheme：config 已水合 → uiTheme 是唯一真值', () => {
  it('显式 dark → dark（系统浅色也不动摇；种子相反也不动摇）', () => {
    expect(resolveTheme({ uiTheme: 'dark', seed: 'light', systemDark: false })).toBe('dark');
  });

  it('显式 light → light（Settings 选浅色即刻生效的那条腿）', () => {
    expect(resolveTheme({ uiTheme: 'light', seed: 'dark', systemDark: true })).toBe('light');
  });

  it("'system' → 跟随 prefers-color-scheme，且**不再**看种子（种子是建窗那一刻的旧快照）", () => {
    expect(resolveTheme({ uiTheme: 'system', seed: 'dark', systemDark: false })).toBe('light');
    expect(resolveTheme({ uiTheme: 'system', seed: 'light', systemDark: true })).toBe('dark');
  });
});

describe('resolveTheme：config 未水合 → 回落主进程种子（FOUC 门）', () => {
  it('种子 dark + 系统浅色 → dark —— 回落 `system` 的那一版在这里转红', () => {
    // 真机形态：uiTheme=dark，OS 浅色。种子脚本已把首帧写成 dark；这里若返 light 就是那一闪。
    expect(resolveTheme({ uiTheme: undefined, seed: 'dark', systemDark: false })).toBe('dark');
  });

  it('种子 light + 系统深色 → light（反向同理：显式浅色用户不该闪一格深）', () => {
    expect(resolveTheme({ uiTheme: undefined, seed: 'light', systemDark: true })).toBe('light');
  });

  it('无种子（注入失败 / 浏览器直开 dist）→ 跟随系统，与 index.html 的 CSS 兜底同口径', () => {
    expect(resolveTheme({ uiTheme: undefined, seed: undefined, systemDark: true })).toBe('dark');
    expect(resolveTheme({ uiTheme: undefined, seed: undefined, systemDark: false })).toBe('light');
  });

  it('种子是垃圾值 → 当作没有种子（不把任意串写进 data-theme）', () => {
    expect(resolveTheme({ uiTheme: undefined, seed: 'DARK', systemDark: false })).toBe('light');
    expect(resolveTheme({ uiTheme: undefined, seed: '', systemDark: true })).toBe('dark');
  });
});

describe('接线：AppShell 真的消费了折算与种子（种子曾是全仓零消费）', () => {
  const shell = readFileSync(fileURLToPath(new URL('./AppShell.tsx', import.meta.url)), 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:])\/\/.*$/gm, '$1');

  it('主题 effect 调 resolveTheme，且喂了种子', () => {
    expect(shell).toMatch(/resolveTheme\(\{/);
    expect(shell).toMatch(/seed:\s*readInitialThemeSeed\(\)/);
  });

  it('不得再把「未水合」折成 system —— `config?.uiTheme ?? \'system\'` 是缺陷原状', () => {
    expect(shell).not.toMatch(/config\?\.uiTheme\s*\?\?\s*'system'/);
  });

  it('水合闸门喂进折算（少了它 uiTheme 缺省与未水合就分不开）', () => {
    expect(shell).toMatch(/uiTheme:\s*configLoaded\s*\?/);
  });
});
