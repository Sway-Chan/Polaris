import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const read = (relative: string): string =>
  readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');

const SCREEN = read('./LogsScreen.tsx');
const CSS = read('../../../styles/index.css');

describe('日志工具栏折行不变量', () => {
  it('来源标签与来源分段在同一个不可拆组，且不再渲染竖分割线', () => {
    expect(SCREEN).toMatch(
      /className="log-filter-group log-source-filter"[\s\S]*?logs\.sourceLabel[\s\S]*?className="seg2"/,
    );
    expect(SCREEN).not.toContain('className="log-tb-sep"');
    expect(CSS).toMatch(/\.log-source-filter\s*\{[^}]*flex-wrap:\s*nowrap/);
  });

  it('诊断操作属于日志级别组，不再作为第三块单独占行', () => {
    expect(SCREEN).toMatch(
      /className="log-filter-group log-level-filter"[\s\S]*?log-diagnostic-toggle[\s\S]*?<\/div>\s*<div className="log-filter-group log-source-filter"/,
    );
    expect(SCREEN).not.toContain('logs.diagnosticLevel');
    expect(CSS).not.toMatch(/\.log-diagnostic-toggle\s*\{[^}]*margin-left:\s*auto/);
  });

  it('底栏把直播、行数与恢复入口组成左侧流状态簇，避开右下 toast', () => {
    expect(SCREEN).toMatch(/className="log-foot"[\s\S]*?className="log-stream-state"[\s\S]*?className="log-live"[\s\S]*?className="log-count"/);
    expect(CSS).toMatch(/\.log-foot\s*\{[^}]*justify-content:\s*flex-start/);
  });
});

describe('会话诊断模式接线', () => {
  it('读写后端进程态并把显示门槛临时投影为 DEBUG', () => {
    expect(SCREEN).toMatch(/api\.logs\s*\.diagnosticState\(\)/);
    expect(SCREEN).toMatch(/api\.logs\s*\.setDiagnostic\(/);
    expect(SCREEN).toContain("diagnosticMode ? 'debug' : level");
  });

  it('诊断切换不经过持久配置或暂存层', () => {
    const start = SCREEN.indexOf('const onToggleDiagnostic = useCallback');
    const end = SCREEN.indexOf('\n  }, [diagnosticBusy', start);
    expect(start).toBeGreaterThan(-1);
    expect(end).toBeGreaterThan(start);
    const body = SCREEN.slice(start, end);
    expect(body).not.toContain('saveConfig');
    expect(body).not.toContain('stage(');
  });
});
