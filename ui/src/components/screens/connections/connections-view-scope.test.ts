/**
 * 连接页「哪些东西只属于明细视图」的守卫 —— 工具栏三个控件 + detail 订阅腿。
 *
 * 两件事同源：搜索 / 暂停 / 关闭全部只作用于明细表，而 detail 那条 1s 全量连接快照的产物
 * （`rows` / `filteredRows` / `total`）也只有明细表消费。默认视图改成拓扑之后，前者是「进页第一眼全是
 * 空按钮」，后者是「每进一次连接页白付一份序列化 + IPC」。故一并 gate 在 `view === 'table'`。
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

describe('工具栏：只属于明细表的控件不进拓扑视图', () => {
  /**
   * 搜索框 / 暂停 / 关闭全部三个必须落在 `view === 'table'` 的条件渲染**之内**。
   *
   * 断在 `.conn-toolbar` 片段内，免得被页面别处的同名条件（如 `#conn-table-view` 的 `hidden`）带偏；
   * 用 id / 回调锚点而不是文案 key —— 换文案不误伤，挪出 gate 必红。
   *
   * 变异对照：删掉 `{view === 'table' && (` 这一层（三个恒渲染）→ 转红；
   * 把其中任一个挪到 gate 外面 → 转红；把条件改成 `view === 'top'` → 转红。
   *
   * 抓不到的：CSS 层面把它们又显示回来（本仓 `.conn-toolbar` 无此类规则）、
   * 以及「渲染了但看不见」这类视觉问题——那要真机。
   */
  it('搜索 / 暂停 / 关闭全部包在 view === table 的条件渲染里', () => {
    const start = SRC.indexOf('className="conn-toolbar"');
    expect(start).toBeGreaterThan(-1);
    const toolbar = SRC.slice(start, SRC.indexOf('id="conn-table-view"', start));

    const guard = toolbar.indexOf("{view === 'table' && (");
    expect(guard, '工具栏里没有 view === table 的条件渲染').toBeGreaterThan(-1);
    const fragOpen = toolbar.indexOf('<>', guard);
    const fragClose = toolbar.indexOf('</>', guard);
    expect(fragOpen).toBeGreaterThan(guard);
    expect(fragClose).toBeGreaterThan(fragOpen);

    for (const [what, anchor] of [
      ['搜索框', 'id="conn-search"'],
      ['暂停按钮', 'id="conn-pause-btn"'],
      ['关闭全部', 'CLOSE_ALL_KEY, () => void onCloseAll()'],
      ['关闭筛选命中', 'CLOSE_FILTERED_KEY, () => void onCloseFiltered()'],
    ] as const) {
      const at = toolbar.indexOf(anchor);
      expect(at, `${what} 不在工具栏里`).toBeGreaterThan(-1);
      expect(at, `${what} 落在 view === table 的 gate 之外`).toBeGreaterThan(fragOpen);
      expect(at, `${what} 落在 view === table 的 gate 之外`).toBeLessThan(fragClose);
    }
  });

  /**
   * 两个 tab 按钮**不**在那层 gate 里（否则切到拓扑就再也切不回来）。
   *
   * 变异对照：把 gate 往上挪、连 `.sub-tabs` 一起包进去 → 本条转红。
   */
  it('视图 tab 本身不受 gate 影响', () => {
    const start = SRC.indexOf('className="conn-toolbar"');
    const toolbar = SRC.slice(start, SRC.indexOf('id="conn-table-view"', start));
    const guard = toolbar.indexOf("{view === 'table' && (");
    expect(toolbar.indexOf("setView('top')")).toBeLessThan(guard);
    expect(toolbar.indexOf("setView('table')")).toBeLessThan(guard);
  });
});

describe('detail 订阅腿随明细视图开关', () => {
  /**
   * detail 腿的 gate 必须含 `view`，且 `view` 必须在依赖数组里。
   *
   * 只有表视图消费它的产物；拓扑视图下继续订阅 = 后端 1s 一帧全量连接快照的序列化 + IPC 全白付。
   * 依赖数组那半条同样关键：只加守卫不加依赖，effect 不会在切视图时重跑，gate 形同虚设。
   *
   * 变异对照：把守卫改回 `if (paused) return;` → 第一条转红；
   * 依赖数组去掉 `view`（留守卫）→ 第二条转红。
   */
  it('gate = paused + view === table，且 view 在依赖数组里', () => {
    const at = SRC.indexOf("api.stats.subscribe('detail')");
    expect(at).toBeGreaterThan(-1);
    const head = SRC.slice(SRC.lastIndexOf('useEffect(', at), at);
    expect(head).toContain('paused');
    expect(head, "detail 腿没有 gate 在 view === 'table'").toContain("view !== 'table'");

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
  it('退订时不清空行缓存', () => {
    const at = SRC.indexOf("api.stats.subscribe('detail')");
    const tail = SRC.slice(at, SRC.indexOf(']);', at));
    expect(tail).not.toContain('setRows([])');
    expect(tail).toContain('sub.dispose()');
  });
});
