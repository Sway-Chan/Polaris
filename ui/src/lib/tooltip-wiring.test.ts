/**
 * G10 —— tooltip 形态守卫：**DOM 上不许再出现原生 `title=`**，一律走统一 tooltip 引擎（`data-tip`）。
 *
 * # 守的是什么
 *
 * 原型 `proto:195-198` 带署名地把原生 `title=` 成建制迁走了（*replaces … all native title=，§4 migration*）。
 * 本仓此前 113 处仍在用原生 `title=`，缺四条用户可感知的能力（skip-delay / 方向可控 / **键盘焦点可见** /
 * 跟随深色主题，逐条见 `tooltip-engine.ts` 头注）。**其中第三条是无障碍缺陷**：原生 `title` 从不在键盘
 * 焦点上显示，键盘与读屏用户完全拿不到。
 *
 * 迁移是一次性的，**增量才是长期风险**：下一个人顺手写 `title="删除"` 不会让任何测试转红，形态就这么
 * 一处一处漏回去。这个门就是挡增量的那道闸。
 *
 * # 为什么是源码结构守卫
 *
 * 本仓 vitest 是 `environment:'node'`（`vite.config.ts`），无 jsdom / 无 CSSOM，**有意为之** ⇒ 渲染不了
 * 组件，「这个按钮 hover 出没出 tip」在这一层根本不可观测。但「源码里有没有 `title=`」是纯文本可断言的，
 * 且正是缺陷复发的那一层。引擎自身的纯逻辑（延迟状态机 / 方向选择 / clamp）在
 * `tooltip-engine.test.ts` + `overlay-position.test.ts` 直测，真机才能判的（实际出屏、WKWebView 表现、
 * RTL 锚定）不在此门射程内。
 *
 * # 判据面 = JSX 属性位的 `title=`，按宿主标签分流
 *
 *  - **小写标签**（`<button>` / `<span>` / `<img>` …）= 真的会渲染成 DOM `title` 属性 → **违规**。
 *  - **大写标签**（`<Modal>` / `<Phead>` / `<Fold>`）= 组件 prop，`title` 在那里是**可见标题文本**，
 *    不是 tooltip → 放行。但组件若把 prop 透传到 DOM，就是「洗一道手的原生 title」⇒ 见
 *    `FORWARDS_TITLE_TO_DOM`：这些组件不许再收 `title=`（它们已改收 `tip` / `data-tip`）。
 */
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SRC = fileURLToPath(new URL('..', import.meta.url));

/** 递归收集前端生产源码（排测试文件——测试里的违规样本是字符串字面量，扫它等于自己判自己违规）。 */
function collectSources(dir: string, acc: string[] = []): string[] {
  for (const e of readdirSync(dir)) {
    if (e === 'node_modules' || e === 'dist') continue;
    const full = join(dir, e);
    if (statSync(full).isDirectory()) collectSources(full, acc);
    else if (/\.tsx?$/.test(e) && !/\.(test|spec)\.tsx?$/.test(e)) acc.push(full);
  }
  return acc;
}

const FILES = collectSources(SRC).map((f) => ({
  rel: relative(SRC, f).split(sep).join('/'),
  src: readFileSync(f, 'utf8'),
}));

/**
 * ── 自曝 ①：扫描面塌了就在**模块加载期**炸 ──
 *
 * 「DOM title 为 0」这个断言在**空集合上恒真**。递归收集一旦坏掉（或目录改名），本门会安静地
 * 全绿并从此不再守任何东西 —— 那比没有门更糟。锚点逐个点名本门真正要守的文件。
 */
const ANCHORS = [
  'lib/tooltip-engine.ts',
  'lib/overlay-position.ts',
  'App.tsx',
  'components/layout/Sidebar.tsx',
  'components/screens/nodes/NodeCard.tsx',
  'components/screens/home/HomeScreen.tsx',
  'components/screens/rules/RuleItem.tsx',
  'components/screens/logs/LogsScreen.tsx',
  'components/screens/resources/ResourcesScreen.tsx',
  'components/screens/settings/primitives.tsx',
  'components/screens/connections/ConnectionsScreen.tsx',
  'components/dialogs/Modal.tsx',
] as const;

if (FILES.length < 100) {
  throw new Error(`[tooltip-wiring] 只扫到 ${FILES.length} 个源文件 —— 扫描面已塌，本守卫失去判据`);
}
for (const a of ANCHORS) {
  if (!FILES.some((f) => f.rel === a)) {
    throw new Error(`[tooltip-wiring] 锚点文件缺失：${a} —— 被改名/移走了？先修判据面再谈绿`);
  }
}

function get(rel: string): string {
  const hit = FILES.find((f) => f.rel === rel);
  if (!hit) throw new Error(`取材失败：${rel}`);
  return hit.src;
}

/** 注释原地抹成空格（保留偏移）——本仓注释惯常逐字引用被替换掉的旧形态，扫原文会被说明文字误伤。 */
function blankComments(src: string): string {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
    .replace(/(^|[^:])\/\/[^\n]*/g, (m, p1: string) => p1 + ' '.repeat(m.length - p1.length));
}

/**
 * 找出属性 `idx` 所属的 JSX 标签名；不在属性位（如 `const title = …`）返回 null。
 *
 * 向前扫到最近的、尚未被关闭的 `<`：途中遇到成对的 `()`/`{}` 跳过（属性值里的表达式），
 * 遇到未配对的 `>` 说明前一个标签已收口 ⇒ 当前位置不是属性位。
 */
function owningTag(src: string, idx: number): string | null {
  let paren = 0;
  let brace = 0;
  for (let i = idx - 1; i >= 0; i--) {
    const c = src[i];
    if (c === ')') paren++;
    else if (c === '(') {
      if (paren === 0) return null;
      paren--;
    } else if (c === '}') brace++;
    else if (c === '{') {
      if (brace === 0) return null;
      brace--;
    } else if (paren === 0 && brace === 0) {
      if (c === '>') return null;
      if (c === '<') return /^<\/?([A-Za-z][\w.]*)/.exec(src.slice(i, i + 40))?.[1] ?? null;
    }
  }
  return null;
}

interface Attr {
  rel: string;
  line: number;
  tag: string;
}

/** 扫全仓某个 JSX 属性名的所有出现，按宿主标签归类。 */
function scanAttr(name: string): Attr[] {
  const out: Attr[] = [];
  const re = new RegExp(`\\b${name}\\s*=`, 'g');
  for (const { rel, src } of FILES) {
    const clean = blankComments(src);
    let m: RegExpExecArray | null;
    while ((m = re.exec(clean))) {
      const tag = owningTag(clean, m.index);
      if (tag) out.push({ rel, line: clean.slice(0, m.index).split('\n').length, tag });
    }
    re.lastIndex = 0;
  }
  return out;
}

const TITLE_ATTRS = scanAttr('title');
const DOM_TITLE = TITLE_ATTRS.filter((a) => /^[a-z]/.test(a.tag));
const PROP_TITLE = TITLE_ATTRS.filter((a) => /^[A-Z]/.test(a.tag));
const DATA_TIP = scanAttr('data-tip');

/**
 * ── 自曝 ②：解析器还活着的正向对照 ──
 *
 * `DOM_TITLE` 为空有两种可能：真的迁干净了，或 `owningTag`/`blankComments` 坏了一个字符都没解析出来。
 * 后者同样给全绿。故正向断言解析器**仍看得见**合法的组件 prop 位 `title=`（`<Modal>`/`<Phead>`/`<Fold>`
 * 那 30+ 处，它们本就该留着）—— 一起归零 = 解析器塌了，不是迁移做完了。
 */
if (PROP_TITLE.length < 20) {
  throw new Error(
    `[tooltip-wiring] 只解析出 ${PROP_TITLE.length} 处组件 prop 位 title= —— ` +
      `属性解析器已塌（<Modal>/<Phead>/<Fold> 本就有 30+ 处），此时「DOM title=0」是假绿`,
  );
}
if (DATA_TIP.length < 60) {
  throw new Error(
    `[tooltip-wiring] 只解析出 ${DATA_TIP.length} 处 data-tip= —— 存量迁移被回滚了，` +
      `或解析器塌了；两种情况本门都不该给绿`,
  );
}

/**
 * ── 署名豁免表 ──
 *
 * 每条必须写明「**为什么这一处非用原生 `title` 不可**」。空表是期望状态：本批 84 处 DOM `title=`
 * 全部迁完，无一处需要豁免。往里加条目 = 承认引擎覆盖不到那个场景，理由要经得起问。
 */
const EXEMPT_DOM_TITLE: ReadonlyArray<{ rel: string; tag: string; reason: string }> = [];

/**
 * 会把 `title` prop 原样透传到 DOM 的组件 —— 它们收 `title` 等于绕过本门洗一道手。
 * 迁移后这些组件改收 `tip`（`primitives.tsx`）或直接由调用方写 `data-tip`（`Button` 走 `{...rest}`）。
 */
const FORWARDS_TITLE_TO_DOM = ['Button', 'Switch', 'Select', 'Segmented', 'InfoIcon'] as const;

describe('G10 · DOM 原生 title= 归零', () => {
  /**
   * 变异对照：把任意一处 `data-tip=` 改回 `title=`（如 `NodeCard.tsx` 的删除钮）→ 本条转红，
   * 报出 `file:line <tag>`。
   */
  it('生产源码里没有任何小写标签带 title=（除署名豁免）', () => {
    const exempt = new Set(EXEMPT_DOM_TITLE.map((e) => `${e.rel}#${e.tag}`));
    const offenders = DOM_TITLE.filter((a) => !exempt.has(`${a.rel}#${a.tag}`)).map(
      (a) => `${a.rel}:${a.line} <${a.tag}>`,
    );
    expect(offenders, '原生 title= 缺 skip-delay / 方向控制 / 键盘可见性 / 深色主题，一律改 data-tip').toEqual([]);
  });

  /** 豁免表里的条目必须真的还在，否则是过期豁免（悄悄放宽了门槛）。 */
  it('豁免表无过期条目', () => {
    for (const e of EXEMPT_DOM_TITLE) {
      expect(
        DOM_TITLE.some((a) => a.rel === e.rel && a.tag === e.tag),
        `豁免 ${e.rel} <${e.tag}> 已无对应代码，请从 EXEMPT_DOM_TITLE 删除`,
      ).toBe(true);
    }
  });

  /**
   * 透传型组件不许再收 `title=`。
   *
   * 变异对照：把 `SettingsUpdate.tsx` 任一 `<Button data-tip=…>` 改回 `title=` → 本条转红。
   */
  it('透传到 DOM 的组件不再收 title= prop', () => {
    const forwarders = new Set<string>(FORWARDS_TITLE_TO_DOM);
    const offenders = PROP_TITLE.filter((a) => forwarders.has(a.tag)).map(
      (a) => `${a.rel}:${a.line} <${a.tag}>`,
    );
    expect(offenders, '这些组件把 title 透传到 DOM —— 收 title= 等于绕过本门').toEqual([]);
  });

  /** 透传型组件本身必须仍存在于 primitives —— 名单过期会让上一条恒绿。 */
  it('透传组件名单未过期（primitives 里逐个还在）', () => {
    const primitives = get('components/screens/settings/primitives.tsx');
    for (const name of FORWARDS_TITLE_TO_DOM) {
      expect(primitives, `${name} 在 primitives.tsx 已不存在，请更新 FORWARDS_TITLE_TO_DOM`).toMatch(
        new RegExp(`export function ${name}\\b`),
      );
    }
  });
});

describe('G10 · 引擎真的接上了（否则 84 处 data-tip 全是哑属性）', () => {
  const app = blankComments(get('App.tsx'));

  /**
   * 这是本门最要紧的一条：把 `title=` 换成 `data-tip=` 而引擎没挂 = **比迁移前更差**
   * （原来至少有原生 tip，现在什么都没有）。
   *
   * 变异对照：删掉 App.tsx 那句 `useEffect(() => initTooltips(), [])` → 本条转红。
   */
  it('App.tsx 挂了 initTooltips 并接住拆卸函数', () => {
    expect(app).toMatch(/import\s*\{[^}]*\binitTooltips\b[^}]*\}\s*from\s*'\.\/lib\/tooltip-engine'/);
    expect(app, 'useEffect 必须 return 拆卸函数，否则 StrictMode 双挂会留下重复监听').toMatch(
      /useEffect\(\s*\(\)\s*=>\s*initTooltips\(\)\s*,\s*\[\]\s*\)/,
    );
  });

  /**
   * 引擎必须真的监听键盘焦点 —— 这是原生 `title` 缺的那条，迁完反而更差是本次最大的倒退风险。
   *
   * 变异对照：删掉 `focusin` 监听 → 本条转红。
   */
  it('引擎监听 focusin 且只认 :focus-visible（键盘可达，鼠标按下不弹）', () => {
    const engine = blankComments(get('lib/tooltip-engine.ts'));
    expect(engine).toMatch(/addEventListener\('focusin'/);
    expect(engine).toMatch(/:focus-visible/);
  });

  /**
   * 读屏不倒退：原生 `title` 是会被播报的，换成自绘浮层后必须由 `aria-describedby` 顶上。
   *
   * 变异对照：删掉 `setAttribute('aria-describedby', …)` → 本条转红。
   */
  it('显示期给触发元素挂 aria-describedby（原生 title 的播报能力不能丢）', () => {
    const engine = blankComments(get('lib/tooltip-engine.ts'));
    expect(engine).toMatch(/setAttribute\(\s*'aria-describedby'/);
    expect(engine, '隐藏时必须还原/移除，否则指向一个不存在的 tip').toMatch(
      /removeAttribute\(\s*'aria-describedby'\s*\)/,
    );
  });

  /**
   * `#tip` 的 CSS 早在仓里（`prototype.css:203-208`）却零消费方 —— 本批把它接上。
   * 断言两侧同一个 id，避免哪天引擎改了 id 而 CSS 没跟着改（表现是 tip 裸奔无样式）。
   */
  it('#tip CSS 与引擎的 TIP_ELEMENT_ID 对得上（死 CSS 已有消费方）', () => {
    const css = readFileSync(fileURLToPath(new URL('../styles/prototype.css', import.meta.url)), 'utf8');
    expect(css).toMatch(/^#tip\s*\{/m);
    expect(css).toMatch(/^#tip\.show\s*\{/m);
    expect(get('lib/tooltip-engine.ts')).toMatch(/TIP_ELEMENT_ID\s*=\s*'tip'/);
  });
});
