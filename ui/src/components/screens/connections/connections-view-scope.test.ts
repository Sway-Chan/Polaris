/**
 * 连接页三视图作用域守卫 —— 共用列表控件、活动动作、已结束动作与各自订阅腿。
 *
 * 搜索只属于活动/已结束列表；暂停与关闭只属于活动连接；清空只属于已结束历史。detail 数据也只在
 * 活动视图消费。默认拓扑下这些控件与数据链必须全部卸载。
 *
 * 为什么是源码结构守卫：本仓 vitest 是 node 环境、全仓无组件渲染测试
 * （见 `connections-context-menu.test.ts` 头注）。条件渲染与 effect 的 gate 都是 JSX / 依赖数组层的
 * 事实，逻辑单测照不出来。
 *
 * **会误伤的改法**（不是 bug，是本守卫要求的形态）：把那段条件渲染从 `{cond && (<>…</>)}` 换成
 * 别的包法（如包一层 `<div>`、或拆成三个独立 gate），第一条会红——届时按新形态改断言，别把 gate 删了。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/** 去注释后的源码：注释里逐字写着被守的条件，扫原文会让「改了 JSX、留着注释」照样绿。 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const SRC = code(
  readFileSync(fileURLToPath(new URL('./ConnectionsScreen.tsx', import.meta.url)), 'utf8'),
);
const STYLE = code(
  readFileSync(fileURLToPath(new URL('../../../styles/prototype.css', import.meta.url)), 'utf8'),
);
const LEGACY_STYLES = code(
  [
    '../../../styles/components.css',
    '../../../styles/index.css',
  ].map((path) => readFileSync(fileURLToPath(new URL(path, import.meta.url)), 'utf8')).join('\n'),
);

describe('工具栏：列表控件不进拓扑，动作不跨生命周期', () => {
  /**
   * 搜索框必须落在列表 gate 内，活动动作与历史动作还要各自落在更窄的生命周期 gate 内。
   *
   * 断在 `.conn-toolbar` 片段内，免得被页面别处的同名条件（如 `#conn-table-view` 的 `hidden`）带偏；
   * 用 id / 回调锚点而不是文案 key —— 换文案不误伤，挪出 gate 必红。
   *
   * 变异对照：删掉列表 gate，或把任一动作挪进错误生命周期 → 转红。
   *
   * 抓不到的：CSS 层面把它们又显示回来（本仓 `.conn-toolbar` 无此类规则）、
   * 以及「渲染了但看不见」这类视觉问题——那要真机。
   */
  it('搜索属于两种列表，活动关闭与历史清空各归各位', () => {
    const start = SRC.indexOf('className="conn-toolbar"');
    expect(start).toBeGreaterThan(-1);
    const toolbar = SRC.slice(start, SRC.indexOf('id="conn-table-view"', start));

    const guard = toolbar.indexOf("{(view === 'active' || view === 'closed') && (");
    expect(guard, '工具栏里没有列表视图条件渲染').toBeGreaterThan(-1);
    const fragOpen = toolbar.indexOf('<>', guard);
    const fragClose = toolbar.indexOf('</>', guard);
    expect(fragOpen).toBeGreaterThan(guard);
    expect(fragClose).toBeGreaterThan(fragOpen);

    for (const [what, anchor] of [['搜索框', 'id="conn-search"']] as const) {
      const at = toolbar.indexOf(anchor);
      expect(at, `${what} 不在工具栏里`).toBeGreaterThan(-1);
      expect(at, `${what} 落在列表 gate 之外`).toBeGreaterThan(fragOpen);
      expect(at, `${what} 落在列表 gate 之外`).toBeLessThan(fragClose);
    }
    const active = toolbar.indexOf("{view === 'active' && <>", guard);
    const closed = toolbar.indexOf("{view === 'closed' &&", guard);
    expect(active).toBeGreaterThan(guard);
    expect(closed).toBeGreaterThan(active);
    for (const anchor of [
      'id="conn-pause-btn"',
      'CLOSE_ALL_KEY, () => void onCloseAll()',
      'CLOSE_FILTERED_KEY, () => void onCloseFiltered()',
    ]) {
      const at = toolbar.indexOf(anchor);
      expect(at).toBeGreaterThan(active);
      expect(at).toBeLessThan(closed);
    }
    expect(toolbar.indexOf('CLEAR_CLOSED_KEY, () => void onClearClosed()')).toBeGreaterThan(closed);
  });

  /**
   * 两个 tab 按钮**不**在那层 gate 里（否则切到拓扑就再也切不回来）。
   *
   * 变异对照：把 gate 往上挪、连 `.sub-tabs` 一起包进去 → 本条转红。
   */
  it('视图 tab 本身不受 gate 影响', () => {
    const start = SRC.indexOf('className="conn-toolbar"');
    const toolbar = SRC.slice(start, SRC.indexOf('id="conn-table-view"', start));
    const guard = toolbar.indexOf("{(view === 'active' || view === 'closed') && (");
    expect(toolbar.indexOf("setView('top')")).toBeLessThan(guard);
    expect(toolbar.indexOf("setView('active')")).toBeLessThan(guard);
    expect(toolbar.indexOf("setView('closed')")).toBeLessThan(guard);
  });
});

describe('detail 订阅腿随明细视图开关', () => {
  /**
   * detail 腿的 gate 必须含 `view`，且 `view` 必须在依赖数组里。
   *
   * 只有表视图消费它的产物；拓扑视图下继续订阅 = 后端连接增量与 IPC 全白付。
   * 依赖数组那半条同样关键：只加守卫不加依赖，effect 不会在切视图时重跑，gate 形同虚设。
   *
   * 变异对照：把守卫改回 `if (paused) return;` → 第一条转红；
   * 依赖数组去掉 `view`（留守卫）→ 第二条转红。
   */
  it('gate = paused + view === active，且 view 在依赖数组里', () => {
    const at = SRC.indexOf("api.stats.subscribe('detail')");
    expect(at).toBeGreaterThan(-1);
    const head = SRC.slice(SRC.lastIndexOf('useEffect(', at), at);
    expect(head).toContain('paused');
    expect(head, "detail 腿没有 gate 在 view === 'active'").toContain("view !== 'active'");

    const deps = SRC.slice(SRC.indexOf('}, [', at), SRC.indexOf(']);', at) + 3);
    expect(deps, 'view 不在 detail effect 的依赖数组里').toContain('view');
    expect(deps).toContain('paused');
  });

  /**
   * 重新订阅时清速率记账 —— 退订期没有帧，回来后首帧的 dt = 整个离开/暂停时长，
   * 算出来的速率既不是当前值也不是历史值。
   *
   * 清空的责任在**订阅腿**而不是暂停按钮：切回明细也是一次重订阅，两条路径共用一处清空。
   * 变异对照：把 `prevRef.current.clear()` 从 effect 里删掉（或挪回 `togglePause`）→ 本条转红。
   */
  it('重新订阅时清空速率记账', () => {
    const at = SRC.indexOf("api.stats.subscribe('detail')");
    const head = SRC.slice(SRC.lastIndexOf('useEffect(', at), at);
    expect(head).toContain('prevRef.current.clear()');
  });

  /**
   * 切走**不清** `rows`：暂停走的是同一条退订腿，而暂停的语义恰恰是「把表冻住给我看」；
   * 清空会让那一小段里空表文案说出「暂无活动连接」这句假话，还多一次闪动。
   *
   * 变异对照：在 effect 的 cleanup（或切视图处）加 `setRows([])` → 本条转红。
   */
  it('暂停退订不清行，切走由独立生命周期 effect 释放缓存', () => {
    const at = SRC.indexOf("api.stats.subscribe('detail')");
    const tail = SRC.slice(at, SRC.indexOf(']);', at));
    expect(tail).not.toContain('setRows([])');
    expect(tail).toContain('sub.dispose()');
    expect(SRC).toContain("if (view === 'active') return;");
    expect(SRC).toContain('setRows([])');
  });
});

describe('连接列表内存边界', () => {
  it('列表按视图条件挂载且最多渲染 20 行，不再用超长占位行', () => {
    expect(SRC).toContain("(view === 'active' || view === 'closed') && (");
    expect(SRC).not.toMatch(/id="conn-table-view"\s+hidden=/);
    expect(SRC).toContain('const CONNECTION_PAGE_SIZE = 20');
    expect(SRC).toContain('pageWindow(filteredRows.length, page, CONNECTION_PAGE_SIZE)');
    expect(SRC).toContain('filteredRows.slice(pagination.start, pagination.end)');
    expect(SRC).not.toContain('conn-spacer');
    expect(SRC).not.toContain('bottomSpace');
    expect(SRC).toContain('<ListPager');
    expect(SRC).toContain('<colgroup>');
    expect(SRC).toContain('<col className="c-type" />');
    expect(STYLE).toContain('table-layout:fixed');
    expect(STYLE).toContain('.conn-table-active col.c-type{ width:54px; }');
    expect(STYLE).toContain('.conn-table-closed col.c-type{ width:54px; }');
    expect(STYLE).not.toMatch(/\.conn-table tr\s*\{[^}]*display\s*:\s*grid/);
    expect(STYLE).not.toContain('contain:layout style');
    expect(STYLE).not.toContain('contain:layout paint');
    expect(STYLE).not.toContain('content-visibility:auto');
    expect(STYLE).not.toMatch(/\.conn-table \.c-type\s*\{[^}]*width\s*:\s*1px/);
  });

  it('活动/已结束列宽各自完整覆盖 920px，旧样式层不再保留第二套连接表规则', () => {
    const widths = (view: 'active' | 'closed') => [
      ...STYLE.matchAll(
        new RegExp(`\\.conn-table-${view} col\\.c-[\\w-]+\\s*\\{\\s*width:(\\d+)px`, 'g'),
      ),
    ].map((match) => Number(match[1]));
    const active = widths('active');
    const closed = widths('closed');
    expect(active).toHaveLength(10);
    expect(closed).toHaveLength(9);
    expect(active.reduce((sum, width) => sum + width, 0)).toBe(920);
    expect(closed.reduce((sum, width) => sum + width, 0)).toBe(920);
    expect(LEGACY_STYLES).not.toContain('.conn-table');
  });

  it('切离活动视图释放活动行缓存；暂停仍只退订并保留当前视口', () => {
    expect(SRC).toContain("if (view === 'active') return;");
    expect(SRC).toContain('setRows([])');
    const at = SRC.indexOf("api.stats.subscribe('detail')");
    const effect = SRC.slice(SRC.lastIndexOf('useEffect(', at), SRC.indexOf(']);', at));
    expect(effect).not.toContain('setRows([])');
  });

  it('已结束 topic 走有界增量索引，未变行不重建', () => {
    expect(SRC).toContain('createTopicSubscription<ConnectionsClosedUpdate>');
    expect(SRC).toContain('applyClosedHistoryUpdate(closedIndexRef.current, update)');
    expect(SRC).toContain('cached?.source === source');
    expect(SRC).not.toContain('snapshot.connections.map(({ entry, closedAt })');
    expect(SRC).toContain('closedIndexRef.current.clear()');
    expect(SRC).toContain('closedRowRef.current.clear()');
  });

  it('活动 topic 走代际/序列增量索引，未变行复用对象', () => {
    expect(SRC).toContain('createTopicSubscription<ConnectionsDetailUpdate>');
    expect(SRC).toContain('applyActiveDetailUpdate(');
    expect(SRC).toContain('cached?.source === entry');
    expect(SRC).toContain('clearActiveDetailState(activeIndexRef.current, activeSyncRef.current)');
    expect(SRC).not.toContain('snap.connections.map');
  });
});

describe('M8 显示迟滞接线（连接表每秒换串是 graphics 表面爆炸的直接驱动）', () => {
  it('applyDetailUpdate 对 rate.d / rate.u / total 三值走 stickyDisplay', () => {
    const at = SRC.indexOf('const applyDetailUpdate');
    expect(at).toBeGreaterThan(0);
    const body = SRC.slice(at, SRC.indexOf('setActiveLoaded(true);', at));
    expect((body.match(/stickyDisplay\(/g) ?? []).length).toBe(3);
    expect(body).toContain('sticky.get(entry.id)');
    expect(body).toContain('sticky.set(entry.id,');
    // total 用更紧的 1/64 迟滞（对齐 fmtBytes 粒度），rate 用默认 1/16。
    expect(body).toContain('stickyDisplay(shown.t, up + dn, 64)');
  });

  it('迟滞缓存生命周期与行缓存同拍（reset / 移除 / 切走清理）', () => {
    expect(SRC).toContain('activeStickyRef.current.clear()');
    expect(SRC).toContain('activeStickyRef.current.delete(id)');
  });
});
