import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

function read(relative: string): string {
  return readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');
}

describe('W11 Windows/Linux 窗口拖动带', () => {
  const appShell = read('./AppShell.tsx');
  const sidebar = read('./Sidebar.tsx');
  const settingsSidebar = read('../screens/settings/SettingsSidebar.tsx');
  const styleSources = [read('../../styles/prototype.css'), read('../../styles/components.css')];

  it('复用两列现有的空 drag-region，不用覆盖层吞页面交互', () => {
    expect(appShell).toContain('<div className="main-chrome" data-tauri-drag-region />');
    expect(sidebar).toContain('<div className="side-chrome" data-tauri-drag-region />');
    expect(settingsSidebar).toContain('<div className="side-chrome" data-tauri-drag-region />');
    expect(appShell.indexOf('className="main-chrome"')).toBeLessThan(
      appShell.indexOf('className="main-scroll"')
    );
  });

  it('Windows/Linux 两列都是紧凑 32px，完整覆盖 26px 自绘窗口控制', () => {
    for (const css of styleSources) {
      expect(css).toMatch(
        /:root\[data-os="win"\] \.side-chrome, :root\[data-os="lin"\] \.side-chrome\s*{\s*height:32px;\s*}/
      );
      expect(css).toMatch(
        /:root\[data-os="win"\] \.main-chrome, :root\[data-os="lin"\] \.main-chrome\s*{\s*height:32px;\s*}/
      );
      expect(css).toMatch(/\.winctl\s*{\s*position:absolute;\s*top:3px;/);
    }
  });

  it('macOS 原生标题栏高度保持 36px', () => {
    for (const css of styleSources) {
      expect(css).toMatch(/:root\[data-os="mac"\] \.main-chrome\s*{\s*height:36px;\s*}/);
    }
  });
});
