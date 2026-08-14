import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const read = (relative: string): string =>
  readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');

const SCREEN = read('./LogsScreen.tsx');
const CSS = read('../../../styles/index.css');
const SCREEN_CSS = read('../../../styles/screens.css');

describe('日志工具栏布局不变量', () => {
  it('级别与来源统一使用 GUI 下拉，不再展开成 5+3 个按钮', () => {
    expect(SCREEN).toMatch(/className="log-filter-field log-level-filter"[\s\S]*?<Csel[\s\S]*?LEVEL_SELECT_OPTIONS/);
    expect(SCREEN).toMatch(/className="log-filter-field log-source-filter"[\s\S]*?<Csel[\s\S]*?sourceOptions/);
    expect(SCREEN).not.toContain('className="log-levels"');
    expect(SCREEN).not.toMatch(/className="seg2"[\s\S]*?logs\.sourceAria/);
  });

  it('筛选与诊断任务链固定成一行，诊断和诊断包在同一操作组', () => {
    expect(SCREEN).toMatch(
      /className="log-tb-primary"[\s\S]*?log-level-filter[\s\S]*?log-source-filter[\s\S]*?className="log-diagnostic-actions"[\s\S]*?log-diagnostic-toggle[\s\S]*?logs\.exportDiag/,
    );
    expect(CSS).toMatch(/\.log-tb-primary\s*\{[^}]*display:\s*grid[^}]*grid-template-columns/);
    expect(CSS).toMatch(
      /grid-template-columns:\s*repeat\(2,minmax\(124px,140px\)\)\s+minmax\(0,1fr\)/,
    );
    expect(CSS).not.toMatch(/@container mainc \(max-width: 7\d\dpx\)[\s\S]*?\.log-tb-primary/);
  });

  it('内核写盘级别属于诊断操作组，不再夹在级别与来源筛选之间', () => {
    const levelFilter = SCREEN.indexOf('className="log-filter-field log-level-filter"');
    const sourceFilter = SCREEN.indexOf('className="log-filter-field log-source-filter"');
    const actions = SCREEN.indexOf('className="log-diagnostic-actions"');
    const badge = SCREEN.indexOf('className={`log-core-lvl');
    const diagnostic = SCREEN.indexOf('log-diagnostic-toggle', actions);

    expect(levelFilter).toBeGreaterThan(-1);
    expect(sourceFilter).toBeGreaterThan(levelFilter);
    expect(actions).toBeGreaterThan(sourceFilter);
    expect(badge).toBeGreaterThan(actions);
    expect(diagnostic).toBeGreaterThan(badge);
    expect(SCREEN).toContain('runtimeLevelTone(runtimeView.level)');
    expect(SCREEN_CSS).toMatch(/\.log-core-lvl\.tone-info\s*\{[^}]*--flow/);
    expect(SCREEN_CSS).toMatch(/\.log-core-lvl\.tone-warn\s*\{[^}]*--warn/);
    expect(SCREEN_CSS).toMatch(/\.log-core-lvl\.tone-error\s*\{[^}]*--err/);
  });

  it('内核生命周期跃迁立即重读级别，5s 轮询只做兜底', () => {
    expect(SCREEN).toMatch(/api\.proxy\.onLifecycle\(\(\) => void refreshRuntimeLevel\(\)\)/);
    expect(SCREEN).toMatch(/setInterval\(\(\) => void refreshRuntimeLevel\(\), RUNTIME_LEVEL_POLL_MS\)/);
    expect(SCREEN).toContain('runtimeReadSeqRef');
    expect(SCREEN).toContain("runtimeView.drift ? 'logs.coreLevelPending' : 'logs.coreLevelValue'");
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
