/**
 * webview 系统右键菜单禁用：**判据** + **入口花名册**。
 *
 * 判据部分是真单测（喂假节点，不靠 DOM 环境——判据刻意用结构类型写，见被测模块头注）；
 * 花名册部分沿用本仓源码结构守卫范式（`connections-context-menu.test.ts` 头注：node 环境、
 * 无组件渲染测试，缺口在**调用点**），且清单不手写而是从 `vite.config.ts` 的入口表推导
 * ——新增一个 webview 入口却没接线，本文件即红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { disableNativeContextMenu, isTextEditingTarget } from './native-context-menu';

/** 去注释后的源码：注释里写着被守的调用形态，扫原文会让「删调用、留注释」照样绿。 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

/** `ui/` 目录（本文件在 `ui/src/lib/`）。 */
const UI_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const REPO_ROOT = join(UI_ROOT, '..');

// ── 假 DOM：判据只用到 tagName / type / disabled / isContentEditable / closest / control ──

interface FakeEl {
  tagName: string;
  type?: string;
  disabled?: boolean;
  readOnly?: boolean;
  isContentEditable?: boolean;
  control?: FakeEl | null;
  parent?: FakeEl;
  closest(sel: string): FakeEl | null;
}

interface Spec {
  tag: string;
  type?: string;
  disabled?: boolean;
  readOnly?: boolean;
  contentEditable?: boolean;
  /** `HTMLLabelElement.control` 的替身。 */
  control?: FakeEl | null;
  /** 祖先（建 `<label><svg/></label>` 这类链）。 */
  parent?: FakeEl;
}

/** `closest` 按逗号分隔的 tag 选择器自底向上匹配（真 DOM 语义：含自身）。 */
function el(spec: Spec | string): FakeEl {
  const s: Spec = typeof spec === 'string' ? { tag: spec } : spec;
  const self: FakeEl = {
    tagName: s.tag.toUpperCase(),
    type: s.type,
    disabled: s.disabled,
    readOnly: s.readOnly,
    isContentEditable: s.contentEditable,
    control: s.control,
    parent: s.parent,
    closest(sel: string) {
      const tags = sel.split(',').map((x) => x.trim().toUpperCase());
      for (let n: FakeEl | undefined = self; n; n = n.parent) {
        if (tags.includes(n.tagName)) return n;
      }
      return null;
    },
  };
  return self;
}

describe('isTextEditingTarget —— 放行系统菜单的判据', () => {
  /**
   * 文本控件放行（本次的新增行为：右键粘贴订阅 URL）。
   *
   * 变异对照：把本函数改成 `return false`（= 退回全禁）→ 本条转红。
   */
  it('文本类 input / textarea / contenteditable → 放行', () => {
    for (const type of ['text', 'search', 'url', 'tel', 'email', 'password', 'number']) {
      expect(isTextEditingTarget(el({ tag: 'input', type }))).toBe(true);
    }
    // 无 type 属性的 <input>（规范：getter 归一化为 'text'）。
    expect(isTextEditingTarget(el('input'))).toBe(true);
    expect(isTextEditingTarget(el('textarea'))).toBe(true);
    expect(isTextEditingTarget(el({ tag: 'div', contentEditable: true }))).toBe(true);
    // contenteditable 的**后代**（真 DOM 里 isContentEditable 继承）。
    expect(isTextEditingTarget(el({ tag: 'b', contentEditable: true }))).toBe(true);
  });

  /**
   * 非文本控件**不**放行 —— 这条是「豁免范围扩到所有元素」这个变异的主防线。
   *
   * 它们没有可粘贴的自由文本，右键给的是页面菜单（重新加载 / 检查元素 / 另存为），
   * 正是本模块要消灭的东西。
   *
   * 变异对照：判据改成「是 input 就放行」（去掉 TEXT_INPUT_TYPES 白名单）→ 本条转红。
   */
  it('非文本 input（checkbox / radio / range / …）→ 不放行', () => {
    for (const type of [
      'checkbox',
      'radio',
      'range',
      'color',
      'file',
      'button',
      'submit',
      'reset',
      'image',
      'hidden',
      'date',
      'time',
      'month',
      'week',
      'datetime-local',
    ]) {
      expect(isTextEditingTarget(el({ tag: 'input', type }))).toBe(false);
    }
  });

  /**
   * 普通元素 / 非元素 → 不放行（判据的默认侧）。
   *
   * 变异对照：把兜底 `return host.isContentEditable === true` 改成 `return true` → 本条转红。
   */
  it('普通元素 / null / 非元素 → 不放行', () => {
    expect(isTextEditingTarget(el('div'))).toBe(false);
    expect(isTextEditingTarget(el('tr'))).toBe(false);
    expect(isTextEditingTarget(el('svg'))).toBe(false);
    expect(isTextEditingTarget(el({ tag: 'div', contentEditable: false }))).toBe(false);
    expect(isTextEditingTarget(null)).toBe(false);
    expect(isTextEditingTarget(undefined)).toBe(false);
    expect(isTextEditingTarget({})).toBe(false); // 没有 closest 的对象（document / window 等）
  });

  /**
   * disabled 不放行、readonly 放行。
   *
   * 判据是「这里能不能拿到文本选区」而不是「能不能写入」：disabled 控件拿不到焦点也选不中文本，
   * 浏览器给的本来就是页面菜单；readonly 能选中能复制，那仍是一份文本编辑菜单，而只读框恰恰是
   * 「右键复制」最有用的地方（生成出来的 URL / 密钥）。
   *
   * 变异对照：给判据加一条 `if (host.readOnly) return false` → readonly 那两条转红；
   * 去掉 `!host.disabled` → disabled 那两条转红。
   */
  it('disabled → 不放行；readonly → 放行', () => {
    expect(isTextEditingTarget(el({ tag: 'input', type: 'text', disabled: true }))).toBe(false);
    expect(isTextEditingTarget(el({ tag: 'textarea', disabled: true }))).toBe(false);
    expect(isTextEditingTarget(el({ tag: 'input', type: 'text', readOnly: true }))).toBe(true);
    expect(isTextEditingTarget(el({ tag: 'textarea', readOnly: true }))).toBe(true);
  });

  /**
   * 落点解析：点在**包裹层**上与点在控件里判据一致。
   *
   * 本仓搜索框是 `<label class="input"><svg 放大镜/><input/></label>`（`ConnectionsScreen.tsx`）。
   * `closest` 只向上走祖先、够不到兄弟节点的 input，故靠 `label.control` 解析。
   *
   * 变异对照：去掉 `el.closest('label')?.control` 这一跳 → 前两条转红（点 label 内边距 /
   * 点放大镜图标时不再放行，同一个搜索框两种行为）。
   */
  it('点在 label 的内边距 / 前置图标上 → 与点在 input 里同判', () => {
    const input = el({ tag: 'input', type: 'search' });
    const label = el({ tag: 'label', control: input });
    const icon = el({ tag: 'svg', parent: label });

    expect(isTextEditingTarget(label)).toBe(true);
    expect(isTextEditingTarget(icon)).toBe(true);
    expect(isTextEditingTarget(input)).toBe(true);
  });

  /**
   * label 解析**不是**「见 label 就放行」：解析出来的控件照样过类型白名单。
   *
   * 变异对照：把 label 分支写成「命中 label 即 return true」→ 本条转红。
   */
  it('label 包的是 checkbox → 仍不放行', () => {
    const box = el({ tag: 'input', type: 'checkbox' });
    const label = el({ tag: 'label', control: box });
    expect(isTextEditingTarget(label)).toBe(false);
    expect(isTextEditingTarget(el({ tag: 'span', parent: label }))).toBe(false);
  });

  /**
   * 直接命中的控件优先于 `label.control`（一个 label 若标注多个控件，`control` 只给第一个，
   * 而用户点的是他点的那个）。
   *
   * 变异对照：把两跳的先后颠倒（先 `label.control` 后自身）→ 本条转红。
   */
  it('自身命中的控件优先于 label.control', () => {
    const first = el({ tag: 'input', type: 'text' });
    const label = el({ tag: 'label', control: first });
    const clicked = el({ tag: 'input', type: 'checkbox', parent: label });
    expect(isTextEditingTarget(clicked)).toBe(false);
  });
});

// ── 监听行为 ──

/** 记下 addEventListener 调用的假宿主。 */
function fakeTarget() {
  const calls: Array<{ type: string; handler: EventListener }> = [];
  const target = {
    addEventListener: (type: string, handler: EventListener) => calls.push({ type, handler }),
  } as unknown as EventTarget;
  return { target, calls };
}

describe('disableNativeContextMenu', () => {
  /**
   * 变异对照：把 `e.preventDefault()` 删掉 / 换成 `e.stopPropagation()` → 转红。
   * preventDefault 才是否决浏览器默认菜单的那一步。
   */
  it('在 contextmenu 上挂监听，非文本落点否决默认动作', () => {
    const { target, calls } = fakeTarget();
    disableNativeContextMenu(target);

    expect(calls.map((c) => c.type)).toEqual(['contextmenu']);

    let prevented = false;
    calls[0].handler({
      target: el('tr'),
      preventDefault: () => (prevented = true),
    } as unknown as Event);
    expect(prevented).toBe(true);
  });

  /**
   * 文本落点**不**否决默认动作 —— 系统的文本编辑菜单（含粘贴）照弹。
   *
   * 变异对照：把 handler 里的 `if (isTextEditingTarget(e.target)) return;` 删掉（退回全禁）→ 转红。
   */
  it('文本落点放行系统菜单（右键粘贴）', () => {
    const { target, calls } = fakeTarget();
    disableNativeContextMenu(target);

    let prevented = false;
    calls[0].handler({
      target: el({ tag: 'input', type: 'search' }),
      preventDefault: () => (prevented = true),
    } as unknown as Event);
    expect(prevented).toBe(false);
  });

  /**
   * 不许升级成 stopPropagation —— 那会在冒泡路上截断事件，自绘菜单（挂在 React 根容器上的委托
   * 监听）就再也收不到。判据是「右键这两处仍弹自绘菜单，其它地方什么都不弹」，
   * 截断传播直接违反前半句。变异对照：加一行 `e.stopPropagation()` → 转红。
   */
  it('只否决默认动作，不阻断事件传播', () => {
    const { target, calls } = fakeTarget();
    disableNativeContextMenu(target);

    const seen: string[] = [];
    const base = {
      target: el('tr'),
      preventDefault: () => seen.push('preventDefault'),
    };
    const probe = new Proxy(base, {
      get(t, k: string) {
        if (k in t) return (t as Record<string, unknown>)[k];
        return () => seen.push(k);
      },
    });
    calls[0].handler(probe as unknown as Event);

    expect(seen).toEqual(['preventDefault']);
  });
});

// ── 入口花名册 ──

describe('每个 webview 入口都禁了系统右键菜单', () => {
  /**
   * 前端入口清单**不手写**：从 `vite.config.ts` 的 `rollupOptions.input` 推导。
   *
   * 每个 webview 各是一个 document，一个窗的监听盖不到别的窗 ⇒ 每个入口必须各调一次。
   * 手写清单的毛病是「新增第四个入口」这件事本身不会让任何断言转红；从构建配置推导就会。
   *
   * 变异对照：摘掉任一入口的 `disableNativeContextMenu()` 调用 → 本条转红（消息里带文件名）；
   * 往 `vite.config.ts` 加一个新 html 入口而不接线 → 也转红。
   */
  it('vite 多入口里每个 html 的入口模块都调用了它', () => {
    const viteCfg = readFileSync(join(UI_ROOT, 'vite.config.ts'), 'utf8');
    const at = viteCfg.indexOf('input: {');
    expect(at).toBeGreaterThan(-1);
    // input 对象的值都是 `path.resolve(__dirname, 'x.html')`，无嵌套花括号 ⇒ 首个 `}` 即块尾。
    const block = viteCfg.slice(at, viteCfg.indexOf('}', at));
    const htmls = [...block.matchAll(/'([^']+\.html)'/g)].map((m) => m[1]);
    // 兜底：清单抠空了会让本条空转成绿，故先钉住已知规模（主窗 + 托盘 + 更新弹窗）。
    expect(htmls.length).toBeGreaterThanOrEqual(3);

    for (const html of htmls) {
      const doc = readFileSync(join(UI_ROOT, html), 'utf8');
      const src = /<script[^>]+src="\/(src\/[^"]+)"/.exec(doc)?.[1];
      expect(src, `${html} 未找到入口模块`).toBeTruthy();
      const entry = code(readFileSync(join(UI_ROOT, src as string), 'utf8'));
      expect(entry, `${html} → ${src} 未调用 disableNativeContextMenu()`).toContain(
        'disableNativeContextMenu()',
      );
    }
  });

  /**
   * 第四个 webview：sing-box 官方面板窗。
   *
   * 它加载的是第三方产物（`scripts/fetch-dashboard.mjs` 拉的 zip、由核 serve），**改不了它的 JS**，
   * 只能由 Rust 侧经 `initialization_script`（document-start 同源执行）从外面挂同一条监听。
   *
   * 变异对照：删掉 `.initialization_script(DISABLE_CONTEXT_MENU_SCRIPT)` 那一行 → 本条转红。
   */
  it('面板窗（Rust 建窗）注入了禁用脚本', () => {
    const body = code(readFileSync(join(REPO_ROOT, 'src-tauri/src/commands/misc.rs'), 'utf8'));
    expect(body).toContain('.initialization_script(DISABLE_CONTEXT_MENU_SCRIPT)');
    expect(body).toContain("document.addEventListener('contextmenu'");
    expect(body).toContain('e.preventDefault()');
  });

  /**
   * 跨语言 parity：Rust 那份是判据的**第二份实现**（TS 那份跑不进面板窗），只改一边即漂移。
   *
   * 把两边的 input 类型白名单抠出来比对 —— 那是两份实现里最容易各改各的一处，也正是「豁免范围」
   * 的实质。变异对照：往任一侧的白名单增删一个 type → 本条转红。
   *
   * 抓不到的：落点解析顺序 / disabled / contenteditable 三个分支的逐字等价（只有真机能验）。
   */
  it('Rust 侧脚本与 TS 判据的 input 类型白名单一致', () => {
    const pick = (src: string, start: string, end: string): string[] => {
      const a = src.indexOf(start);
      expect(a, `未找到 ${start}`).toBeGreaterThan(-1);
      const chunk = src.slice(a + start.length, src.indexOf(end, a + start.length));
      return [...chunk.matchAll(/'([a-z-]+)'/g)].map((m) => m[1]).sort();
    };
    const ts = readFileSync(join(UI_ROOT, 'src/lib/native-context-menu.ts'), 'utf8');
    const rs = readFileSync(join(REPO_ROOT, 'src-tauri/src/commands/misc.rs'), 'utf8');

    const tsTypes = pick(ts, 'TEXT_INPUT_TYPES = new Set([', ']);');
    const rsTypes = pick(rs, 'var T=[', '];');
    expect(tsTypes.length).toBeGreaterThanOrEqual(7);
    expect(rsTypes).toEqual(tsTypes);
  });
});
