/**
 * tooltip 引擎的 **DOM 行为**门 —— 只钉三条「源码文本断言证明不了」的事实：
 *
 *  1. **键盘焦点真的会弹 tip**（原生 `title` 缺的那条），且只认 `:focus-visible`（鼠标按下不弹）；
 *  2. **`aria-describedby` 挂了又还原**（原生 `title` 会被读屏播报，换成自绘浮层后必须由它顶上，
 *     否则这次迁移对读屏用户是**净倒退**）；
 *  3. **拆卸后不再响应**（引擎挂在 App 的 useEffect 上，StrictMode 会 mount→unmount→mount）。
 *
 * `tooltip-wiring.test.ts` 只能断言「源码里有 `focusin` 这个字符串」——那挡不住逻辑写反（比如
 * `:focus-visible` 判反、`aria-describedby` 只挂不还原）。故这一层必须真的驱动一遍事件。
 *
 * # node 环境的桩
 *
 * 本仓 vitest 是 `environment:'node'` 无 jsdom（`vite.config.ts`，有意为之）。沿用
 * `i18n/language-hydration.test.ts` 的先例：**先立桩再 `await import()`**。桩只实现引擎真正用到的
 * 那几个面（`closest` / `dataset` / `classList` / `getBoundingClientRect` / 属性存取），
 * 不是通用 DOM 实现——够用即止，多写一行都是负债。
 *
 * 布局相关的（实际出不出屏、WKWebView 表现、RTL 锚定）此处**测不了也不测**，属真机验证项。
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

/* ── 最小 DOM 桩 ───────────────────────────────────────────────────────── */

class FakeNode {}

type Listener = (e: unknown) => void;

class FakeElement extends FakeNode {
  readonly tag: string;
  readonly attrs = new Map<string, string>();
  readonly dataset: Record<string, string | undefined> = {};
  readonly style: Record<string, string> = {};
  readonly childNodes: FakeElement[] = [];
  parentElement: FakeElement | null = null;
  isConnected = true;
  /** `:focus-visible` 的可控开关——桩里没有真实焦点模型，由用例直接摆布。 */
  focusVisible = false;
  textContent = '';
  id = '';
  /** 引擎只用 add/remove/contains，够用即止。 */
  private readonly classes = new Set<string>();
  readonly classList = {
    add: (c: string) => void this.classes.add(c),
    remove: (c: string) => void this.classes.delete(c),
    contains: (c: string) => this.classes.has(c),
  };
  /** 定位输入：用例给触发元素摆一个矩形；tip 自身固定 100×40。 */
  rect = { left: 0, top: 0, width: 0, height: 0 };

  constructor(tag: string) {
    super();
    this.tag = tag;
  }

  getBoundingClientRect() {
    return this.rect;
  }
  setAttribute(k: string, v: string) {
    this.attrs.set(k, v);
  }
  getAttribute(k: string) {
    return this.attrs.get(k) ?? null;
  }
  removeAttribute(k: string) {
    this.attrs.delete(k);
  }
  appendChild(child: FakeElement) {
    child.parentElement?.childNodes.splice(child.parentElement.childNodes.indexOf(child), 1);
    this.childNodes.push(child);
    child.parentElement = this;
  }
  remove() {
    this.parentElement?.childNodes.splice(this.parentElement.childNodes.indexOf(this), 1);
    this.parentElement = null;
  }
  contains(other: unknown): boolean {
    if (other === this) return true;
    return this.childNodes.some((c) => c.contains(other));
  }
  /** 只支持引擎实际用的两个选择器。 */
  matches(sel: string): boolean {
    if (sel === ':focus-visible') return this.focusVisible;
    if (sel === '[data-tip]') return this.dataset.tip !== undefined;
    if (sel === 'dialog') return this.tag === 'dialog';
    return false;
  }
  closest(sel: string): FakeElement | null {
    // eslint-disable-next-line @typescript-eslint/no-this-alias
    let cur: FakeElement | null = this;
    while (cur) {
      if (cur.matches(sel)) return cur;
      cur = cur.parentElement;
    }
    return null;
  }
}

const docListeners = new Map<string, Listener[]>();
const winListeners = new Map<string, Listener[]>();
const push = (m: Map<string, Listener[]>, k: string, fn: Listener) =>
  void m.set(k, [...(m.get(k) ?? []), fn]);
const drop = (m: Map<string, Listener[]>, k: string, fn: Listener) =>
  void m.set(k, (m.get(k) ?? []).filter((f) => f !== fn));

let body: FakeElement;

function installDom(): void {
  body = new FakeElement('body');
  docListeners.clear();
  winListeners.clear();
  const g = globalThis as Record<string, unknown>;
  g.Node = FakeNode;
  g.Element = FakeElement;
  g.document = {
    body,
    createElement: (tag: string) => {
      const el = new FakeElement(tag);
      el.rect = { left: 0, top: 0, width: 100, height: 40 };
      return el;
    },
    addEventListener: (k: string, fn: Listener) => push(docListeners, k, fn),
    removeEventListener: (k: string, fn: Listener) => drop(docListeners, k, fn),
  };
  g.window = {
    innerWidth: 1000,
    innerHeight: 800,
    addEventListener: (k: string, fn: Listener) => push(winListeners, k, fn),
    removeEventListener: (k: string, fn: Listener) => drop(winListeners, k, fn),
  };
}

installDom();

const { initTooltips, TIP_DELAY, TIP_ELEMENT_ID } = await import('./tooltip-engine');

const fire = (kind: string, e: unknown) => (docListeners.get(kind) ?? []).forEach((f) => f(e));
const fireWin = (kind: string, e: unknown) => (winListeners.get(kind) ?? []).forEach((f) => f(e));
/** 当前挂在 body（或 dialog）下的 `#tip`。 */
const tipOf = (host: FakeElement = body) => host.childNodes.find((c) => c.id === TIP_ELEMENT_ID);

/** 造一个挂了 `data-tip` 的触发元素并接进 body。 */
function trigger(tip: string, extra?: Partial<Pick<FakeElement, 'dataset'>>): FakeElement {
  const el = new FakeElement('button');
  el.dataset.tip = tip;
  Object.assign(el.dataset, extra?.dataset ?? {});
  el.rect = { left: 400, top: 300, width: 40, height: 20 };
  body.appendChild(el);
  return el;
}

let teardown: () => void;

beforeEach(() => {
  vi.useFakeTimers();
  installDom();
  teardown = initTooltips();
});

afterEach(() => {
  teardown();
  vi.useRealTimers();
});

describe('键盘可达（原生 title 缺的第 3 条，也是本次最大的倒退风险）', () => {
  /**
   * 变异对照：把 `addEventListener('focusin', …)` 删掉 → 本条转红。
   * 这正是迁移前的状态：键盘用户什么都拿不到。
   */
  it(':focus-visible 焦点立即显示 tip（不等 500ms）', () => {
    const el = trigger('测速');
    el.focusVisible = true;
    fire('focusin', { target: el });
    // 键盘路径不排队：不推进定时器就应该已经出来了。
    expect(tipOf()?.textContent).toBe('测速');
    expect(tipOf()?.classList.contains('show')).toBe(true);
  });

  /**
   * 鼠标按下取得的焦点**不**弹（否则每点一次按钮都糊一个 tip）。
   *
   * 变异对照：把 `!isKeyboardFocus(el)` 的取反去掉 → 本条转红。
   */
  it('非 :focus-visible 的焦点不显示', () => {
    const el = trigger('测速');
    el.focusVisible = false;
    fire('focusin', { target: el });
    expect(tipOf()).toBeUndefined();
  });

  it('失焦即隐藏', () => {
    const el = trigger('测速');
    el.focusVisible = true;
    fire('focusin', { target: el });
    fire('focusout', { target: el });
    expect(tipOf()?.classList.contains('show')).toBe(false);
  });
});

describe('aria-describedby —— 读屏播报能力不能随 title 一起丢', () => {
  /**
   * 变异对照：删掉 `el.setAttribute('aria-describedby', TIP_ELEMENT_ID)` → 本条转红。
   * 那种状态下视觉用户看得到 tip、读屏用户什么都没有 = 净倒退。
   */
  it('显示期指向 #tip，隐藏后移除', () => {
    const el = trigger('复制链接');
    el.focusVisible = true;
    fire('focusin', { target: el });
    expect(el.getAttribute('aria-describedby')).toBe(TIP_ELEMENT_ID);
    fire('focusout', { target: el });
    expect(el.getAttribute('aria-describedby')).toBeNull();
  });

  /**
   * 触发元素原本就有 `aria-describedby` 时**还原原值**，不能吞掉。
   *
   * 变异对照：把 `hide()` 里的还原分支改成无条件 `removeAttribute` → 本条转红。
   */
  it('原有 aria-describedby 被还原而非抹掉', () => {
    const el = trigger('复制链接');
    el.setAttribute('aria-describedby', 'existing-desc');
    el.focusVisible = true;
    fire('focusin', { target: el });
    expect(el.getAttribute('aria-describedby')).toBe(TIP_ELEMENT_ID);
    fire('focusout', { target: el });
    expect(el.getAttribute('aria-describedby')).toBe('existing-desc');
  });
});

describe('hover 路径与 skip-delay', () => {
  it('首次 hover 等满 TIP_DELAY 才出', () => {
    const el = trigger('删除');
    fire('mouseover', { target: el });
    vi.advanceTimersByTime(TIP_DELAY - 1);
    expect(tipOf()).toBeUndefined();
    vi.advanceTimersByTime(1);
    expect(tipOf()?.textContent).toBe('删除');
  });

  /**
   * 连扫相邻图标钮：第二颗**立即**出，不重新等满。
   *
   * 变异对照：把 `tipOpenDelay` 的 `tipOpen ||` 去掉 → 本条转红（= 原生 title 的行为）。
   */
  it('已有 tip 开着时，移到下一个触发器立即换文案', () => {
    const a = trigger('置顶');
    const b = trigger('上移');
    fire('mouseover', { target: a });
    vi.advanceTimersByTime(TIP_DELAY);
    expect(tipOf()?.textContent).toBe('置顶');
    fire('mouseover', { target: b });
    vi.advanceTimersByTime(0);
    expect(tipOf()?.textContent).toBe('上移');
  });

  it('空 data-tip 不显示（对应 `data-tip={cond ? x : undefined}` 的关态）', () => {
    const el = trigger('');
    fire('mouseover', { target: el });
    vi.advanceTimersByTime(TIP_DELAY);
    expect(tipOf()).toBeUndefined();
  });

  it('ESC / 滚动 / resize 立即隐藏', () => {
    for (const shut of [
      () => fire('keydown', { key: 'Escape' }),
      () => fireWin('scroll', {}),
      () => fireWin('resize', {}),
    ]) {
      const el = trigger('导出');
      el.focusVisible = true;
      fire('focusin', { target: el });
      expect(tipOf()?.classList.contains('show')).toBe(true);
      shut();
      expect(tipOf()?.classList.contains('show')).toBe(false);
    }
  });
});

describe('宿主选择与拆卸', () => {
  /**
   * 弹窗是原生 `<dialog>`+`showModal()`（top layer）——tip 挂 body 会被压在弹窗底下看不见。
   *
   * 变异对照：把 `el.closest('dialog') ?? document.body` 换成 `document.body` → 本条转红。
   */
  it('触发器在 <dialog> 内时，tip 挂进那个 dialog 而不是 body', () => {
    const dlg = new FakeElement('dialog');
    body.appendChild(dlg);
    const el = new FakeElement('button');
    el.dataset.tip = '刷新';
    el.rect = { left: 400, top: 300, width: 40, height: 20 };
    dlg.appendChild(el);

    el.focusVisible = true;
    fire('focusin', { target: el });
    expect(tipOf(dlg)?.textContent).toBe('刷新');
    expect(tipOf(body)).toBeUndefined();
  });

  /**
   * 拆卸后事件不再有反应、`#tip` 节点也不留 —— StrictMode 的双挂不该堆监听或留孤儿节点。
   *
   * 变异对照：把 return 的拆卸函数改成 `() => {}` → 本条转红。
   */
  it('拆卸后不再响应，且不留孤儿 #tip', () => {
    const el = trigger('克隆');
    el.focusVisible = true;
    fire('focusin', { target: el });
    expect(tipOf()).toBeDefined();

    teardown();
    expect(tipOf()).toBeUndefined();
    expect(el.getAttribute('aria-describedby')).toBeNull();

    fire('focusin', { target: el });
    expect(tipOf()).toBeUndefined();

    teardown = () => {}; // afterEach 的重复拆卸置空
  });
});
