import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const read = (relative: string): string =>
  readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');

const SCREEN = read('./LogsScreen.tsx');
const CSS = read('../../../styles/index.css');
const PROTO_CSS = read('../../../styles/prototype.css');

describe('日志工具栏布局不变量', () => {
  it('级别与来源统一使用 GUI 下拉，不再展开成 5+3 个按钮', () => {
    expect(SCREEN).toMatch(/className="log-filter-field log-level-filter"[\s\S]*?<Csel[\s\S]*?LEVEL_SELECT_OPTIONS/);
    expect(SCREEN).toMatch(/className="log-filter-field log-source-filter"[\s\S]*?<Csel[\s\S]*?sourceOptions/);
    expect(SCREEN).not.toContain('className="log-levels"');
    expect(SCREEN).not.toMatch(/className="seg2"[\s\S]*?logs\.sourceAria/);
  });

  it('第一行按级别、来源、诊断模式、导出、日志目录排序', () => {
    expect(SCREEN).toMatch(
      /className="log-tb-primary"[\s\S]*?log-level-filter[\s\S]*?log-source-filter[\s\S]*?className="log-primary-actions"[\s\S]*?log-diagnostic-toggle[\s\S]*?className="log-export"[\s\S]*?logs\.exportReport[\s\S]*?logs\.exportLogsOnly[\s\S]*?logs\.openDir/,
    );
    expect(PROTO_CSS).toMatch(/\.log-tb-primary\s*\{[^}]*display:grid[^}]*grid-template-columns/);
    expect(PROTO_CSS).toMatch(
      /grid-template-columns:minmax\(124px,160px\) minmax\(124px,160px\) minmax\(0,1fr\)/,
    );
    expect(SCREEN).toContain('role="menu"');
    expect(SCREEN).toContain('aria-haspopup="menu"');
  });

  it('内核写盘级别仅在不一致或读取失败时以既有 pill 标签跟随诊断模式按钮', () => {
    const levelFilter = SCREEN.indexOf('className="log-filter-field log-level-filter"');
    const sourceFilter = SCREEN.indexOf('className="log-filter-field log-source-filter"');
    const diagnostic = SCREEN.indexOf('log-diagnostic-toggle');
    const badge = SCREEN.indexOf('className="pill warn log-core-lvl"');
    const exportMenu = SCREEN.indexOf('className="log-export"');

    expect(levelFilter).toBeGreaterThan(-1);
    expect(sourceFilter).toBeGreaterThan(levelFilter);
    expect(diagnostic).toBeGreaterThan(sourceFilter);
    expect(badge).toBeGreaterThan(diagnostic);
    expect(badge).toBeLessThan(exportMenu);
    expect(SCREEN).toContain('className="pill warn log-core-lvl"');
    expect(SCREEN).toContain("runtimeView.kind === 'known' && runtimeView.drift");
    expect(SCREEN).toContain("runtimeView.kind === 'unavailable'");
  });

  it('第二行按搜索、自动滚动、脱敏、复制、清空排序，空结果禁用复制与清空', () => {
    expect(SCREEN).toMatch(
      /className="log-tb-main"[\s\S]*?log-tb-search[\s\S]*?toggleFollow[\s\S]*?toggleRedactLogs[\s\S]*?onCopy[\s\S]*?onClearClick/,
    );
    expect(SCREEN).toContain('disabled={visible.length === 0}');
    expect(SCREEN).toContain('disabled={logs.length === 0 && pendingCount === 0 && visible.length === 0}');
    expect(SCREEN).not.toContain('log-tb-utilities');
    expect(PROTO_CSS).not.toContain('.log-tb-sep');
  });

  it('内核生命周期跃迁立即重读级别，5s 轮询只做兜底', () => {
    expect(SCREEN).toMatch(/api\.proxy\.onLifecycle\(\(\) => void refreshRuntimeLevel\(\)\)/);
    expect(SCREEN).toMatch(/setInterval\(\(\) => void refreshRuntimeLevel\(\), RUNTIME_LEVEL_POLL_MS\)/);
    expect(SCREEN).toContain('runtimeReadSeqRef');
    expect(SCREEN).toContain("t('logs.coreLevelPending'");
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
