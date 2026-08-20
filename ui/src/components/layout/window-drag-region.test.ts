import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

function read(relative: string): string {
  return readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');
}

describe('W11 Windows 窗口拖动带', () => {
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

  it('Windows 两列都是 40px，完整覆盖自绘窗口控制高度', () => {
    for (const css of styleSources) {
      expect(css).toMatch(/:root\[data-os="win"\] \.side-chrome\s*{\s*height:40px;\s*}/);
      expect(css).toMatch(/:root\[data-os="win"\] \.main-chrome\s*{\s*height:40px;\s*}/);
    }
  });

  it('macOS 与 Linux 的既有高度语义没有被一并放大', () => {
    for (const css of styleSources) {
      expect(css).toMatch(/:root\[data-os="mac"\] \.main-chrome\s*{\s*height:36px;\s*}/);
      expect(css).toMatch(/:root:not\(\[data-os="mac"\]\) \.side-chrome\s*{\s*height:12px;\s*}/);
      expect(css).not.toMatch(/:root\[data-os="lin"\] \.main-chrome/);
    }
  });
});
