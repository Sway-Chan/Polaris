/**
 * ListEditor 的 draft + onBlur —— 守「逐键落盘」这个缺陷的根因。
 *
 * # 缺陷
 *
 * 旧实现的行输入是受控 `onChange` **立刻回调父级**：`update(idx,v) → onChange(copy)` →
 * `useConfig().update` 漏斗 → `editRoute` → `stage()`。于是**每敲一个字符**跑一遍
 * sanitize+validate、一次原子写盘（tmp+rename）、一次 `broadcast_config_changed` →
 * 一次 `spawn(switch_mode)`。本组件是 7 个挂点的共同源头（`bypassLANList` ×2 /
 * `tunConfig` ×3 / `dnsConfig` / `fakeIpFilterList`，全部落在 29 个核心键内 ⇒ 真进暂存）。
 *
 * `SettingsDns.tsx:235-265` 的三个文本框当初正是这个毛病、已改成 draft+onBlur（那里的头注写着），
 * 本组件是唯一没跟上的。修法**照抄那一套**，不发明第二套。
 *
 * # 射程：为什么这里只测纯函数
 *
 * 本仓 vitest 跑 `environment: 'node'` 且**刻意不装 jsdom / testing-library**
 * （`initial-tab-first-frame.test.tsx` 头注明写「别为这道门破例」）。组件级门的既有手段是
 * `react-dom/server` 的 SSR —— 而 SSR **不跑 effect、也无从派发 onChange/onBlur 事件**，
 * 所以「敲字符不写盘、blur 才写盘」这条交互**在本仓结构上无法单测**。
 *
 * 故这里的分工是：
 *  - 把最容易写错的一格（外部刷新时草稿取什么值）抽成纯函数 `nextDraft`，在此穷举；
 *  - 用下面那组**接线守卫**钉住「行输入的 onChange 不得碰父级、onBlur 必须提交」这个形态；
 *  - 交互本身如实标注为真机确认项（见交付说明），不造一道跑不到那一步的门。
 */
import { readFileSync } from 'node:fs';
import { describe, it, expect } from 'vitest';
import { nextDraft, sameEntries } from './ListEditor';

describe('sameEntries', () => {
  it('长度/顺序/内容任一不同即不相同', () => {
    expect(sameEntries([], [])).toBe(true);
    expect(sameEntries(['a', 'b'], ['a', 'b'])).toBe(true);
    expect(sameEntries(['a'], ['a', 'b'])).toBe(false);
    expect(sameEntries(['a', 'b'], ['b', 'a'])).toBe(false);
    expect(sameEntries(['a'], ['A'])).toBe(false); // 大小写敏感：这是用户输入的原文，不是去重键
  });
});

describe('nextDraft —— 外部改动到达时草稿取什么值', () => {
  // 变异对照：把「草稿≠种子 → 保留草稿」那条删掉 → 本条转红。这是最坏的一种失效：
  // 用户正在敲第三个 CIDR，后台写盘/托盘改动一到，输入框被抹回磁盘值。
  it('用户已改过草稿 ⇒ 外部刷新不得打断（保留草稿）', () => {
    const seed = ['10.0.0.0/8'];
    const cur = ['10.0.0.0/8', '192.168.'];
    expect(nextDraft(cur, seed, ['10.0.0.0/8', '172.16.0.0/12'])).toBe(cur);
  });

  // 变异对照：把这条改成恒保留草稿 → 转红。托盘/备份恢复/另一屏保存的改动就永远回填不进来，
  // 用户看到的是一份陈旧列表（且他一保存就把别人的改动覆盖掉）。
  it('用户没动过草稿 ⇒ 跟随新配置', () => {
    const seed = ['10.0.0.0/8'];
    const incoming = ['10.0.0.0/8', '172.16.0.0/12'];
    expect(nextDraft(seed, seed, incoming)).toBe(incoming);
  });

  // props 身份易变（父级常常每次重渲都新建数组）。内容已相同就必须返回原引用，
  // 否则每次父级重渲都白多一次 setState → 重渲。变异对照：去掉这条早退 → 本条转红。
  it('内容已一致 ⇒ 返回原引用（不为身份变化白重渲）', () => {
    const cur = ['10.0.0.0/8'];
    expect(nextDraft(cur, cur, ['10.0.0.0/8'])).toBe(cur);
  });

  it('空列表两侧与「清空」都不误判', () => {
    const empty: string[] = [];
    expect(nextDraft(empty, empty, [])).toBe(empty);
    const incoming: string[] = [];
    expect(nextDraft(['a'], ['a'], incoming)).toBe(incoming); // 外部把列表清空了，用户没在编辑 → 跟随
  });
});

/**
 * **接线守卫** —— 纯函数对 ≠ 组件真的这么接。交互测不了（见文件头），这组就是替代品：
 * 它钉住的是「打字的那条路上没有父级 onChange」这个**形态**。
 */
describe('接线：行输入打字不得碰父级，blur 才提交', () => {
  /** 去注释后再扫：本文件与组件头注都逐字写着旧形态 `onChange(copy)`，扫原文会被说明文字误伤。 */
  const SRC = readFileSync(new URL('./ListEditor.tsx', import.meta.url), 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/^[ \t]*\/\/.*$/gm, '');

  // 变异对照：把行 onChange 改回 `editRow` → `onChange([...])`（直接回调父级）→ 本条转红。
  it('打字只走 editRow（只动草稿）', () => {
    expect(SRC).toContain('onChange={(e) => editRow(idx, e.target.value)}');
  });

  it('行输入挂了 onBlur 提交 + Enter 触发 blur', () => {
    expect(SRC).toContain('onBlur={() => commit(draft)}');
    expect(SRC).toContain("if (e.key === 'Enter') e.currentTarget.blur();");
  });

  // `editRow` 是唯一不提交的写入口；它绝不能调 commit/onChange，否则等于没改。
  it('editRow 内不得出现 commit / onChange', () => {
    const body = SRC.slice(SRC.indexOf('function editRow('));
    const end = body.indexOf('\n  }');
    const editRowBody = body.slice(0, end);
    expect(editRowBody).not.toContain('commit(');
    expect(editRowBody).not.toContain('onChange(');
  });

  // 离散动作（删/加/导入）必须基于**草稿**：基于 `value` 会丢掉另一行里还没提交的编辑。
  // 变异对照：把任一处的 `draft` 换回 `value` → 本条转红。
  it('删除 / 添加 / 批量导入都基于 draft，不基于 value', () => {
    expect(SRC).toContain('commit(draft.filter((_, i) => i !== idx))');
    expect(SRC).toContain("commit([...draft, ''])");
    expect(SRC).toContain('parseBulkEntries(importDraft, [...draft], max)');
    // 渲染也必须走草稿，否则打字看不见（草稿变了但画的是 value）。
    expect(SRC).toContain('{draft.map((entry, idx) => (');
  });
});
