/**
 * G14 —— 独立窗浮层的**视图态复位**守卫。
 *
 * # 守什么
 *
 * 原型的托盘每次打开都回主视图（`proto:4009 openTray(){ … trayView(false); … }`）—— 浮层是「弹出即用完就收」
 * 的表面，上次停在哪跟这次没关系。实现把托盘做成了**常驻不重建的独立 Tauri 窗**（`src-tauri/src/tray.rs`
 * 「预建一次（隐藏）」），于是 React 组件永不卸载、`useState` 永不重置：上次停在二级视图，下次点托盘图标
 * 大概率还停在二级。这不是渲染问题，是「窗生命周期变了、复位腿没跟着补」。
 *
 * 常驻窗把「组件卸载即复位」这条隐式保障拿掉了，而拿掉它是**静默**的 —— 没有任何测试会因此转红。
 * 本门就是把它变成显式的：浮层里每一个联合字面量 `useState` 都要登记，视图态必须有 show/focus 复位腿。
 *
 * # 判据面
 *
 *  - `ui/src/tray/**` 与 `ui/src/update-popup/**` 的非测试源码；
 *  - 其中形如 `const [x, setX] = useState<'a' | 'b'>('a')` 的**联合字面量 state**；
 *  - 以及 `window.addEventListener('focus', …)` 的处理器体（内联箭头函数或具名 const 均可）。
 *
 * # 什么算「视图态」
 *
 * 联合成员**全是字符串字面量**（不含 `null` / `undefined`）**且默认值是其中之一** ⇒ 视图态，必须有复位腿。
 * 含 `null` 的（如 `pending: 'start' | 'stop' | null`）是「在飞方向」，天然以 `null` 为静息值、不是层级视图；
 * 它们照样进登记表（改成视图态时门会说话），只是不要求复位腿。
 *
 * # 为什么是源码结构守卫
 *
 * 本仓 vitest 是 `environment:'node'`（无 jsdom，有意为之）⇒ 组件渲染不了，「窗再弹出时停在哪一屏」
 * 在这一层不可观测。「focus 处理器里有没有那句复位」是纯文本可断言的，且正是缺陷所在的那一层。
 * **真机仍需看一眼**：窗显隐由 Rust 侧控制，`focus` 事件是否每次弹出都触发要设备上确认。
 */
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SRC = fileURLToPath(new URL('..', import.meta.url));

/** 浮层目录 —— 独立 Tauri 窗承载、组件永不卸载的那几处。 */
const OVERLAY_DIRS = ['tray', 'update-popup'] as const;

function walk(dir: string, acc: string[] = []): string[] {
  for (const e of readdirSync(dir)) {
    if (e === 'node_modules' || e === 'dist') continue;
    const full = join(dir, e);
    if (statSync(full).isDirectory()) walk(full, acc);
    else if (/\.tsx?$/.test(e) && !/\.(test|spec)\.tsx?$/.test(e)) acc.push(full);
  }
  return acc;
}

/** 去注释（注释里逐字写着 `setView('main')` 这类说明，直接扫会把「解释」当成「实现」）。 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const FILES = OVERLAY_DIRS.flatMap((d) => walk(join(SRC, d))).map((f) => ({
  rel: relative(SRC, f).split(sep).join('/'),
  src: code(readFileSync(f, 'utf8')),
}));

// ── 解析器 ──────────────────────────────────────────────────────────────────

interface UnionState {
  file: string;
  name: string;
  setter: string;
  members: string[];
  /** 联合里是否含 `null` / `undefined`（含则不是层级视图，是「在飞/静息」二元态）。 */
  nullable: boolean;
  init: string;
}

function parseUnionStates(rel: string, src: string): UnionState[] {
  const out: UnionState[] = [];
  const re = /const\s*\[\s*(\w+)\s*,\s*(set\w+)\s*\]\s*=\s*useState<([^>]*'[^>]*)>\(([^)]*)\)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(src))) {
    const typeText = m[3];
    const members = [...typeText.matchAll(/'([^']*)'/g)].map((x) => x[1]);
    if (members.length < 2) continue;
    out.push({
      file: rel,
      name: m[1],
      setter: m[2],
      members,
      nullable: /\bnull\b|\bundefined\b/.test(typeText),
      init: m[4].trim(),
    });
  }
  return out;
}

/** 取从 `from` 起第一个 `{` 开始的配平大括号块。 */
function braceBody(src: string, from: number): string {
  const open = src.indexOf('{', from);
  if (open < 0) return '';
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}' && --depth === 0) return src.slice(open, i + 1);
  }
  return '';
}

/** 全部 `window.addEventListener('focus', …)` 处理器体（内联箭头 + 具名 const 两种写法）。 */
function focusHandlerBodies(src: string): string[] {
  const out: string[] = [];
  const re = /window\.addEventListener\(\s*'focus'\s*,\s*/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(src))) {
    const rest = src.slice(m.index + m[0].length);
    const named = /^([A-Za-z_$][\w$]*)\s*\)/.exec(rest);
    if (named) {
      const declAt = src.indexOf(`const ${named[1]} = `);
      if (declAt >= 0) out.push(braceBody(src, declAt));
    } else {
      out.push(braceBody(src, m.index + m[0].length));
    }
  }
  return out;
}

/** 该视图态在某条 focus 腿里被复位回默认值了吗。 */
function hasResetLeg(src: string, st: UnionState): boolean {
  const call = `${st.setter}(${st.init})`;
  return focusHandlerBodies(src).some((b) => b.includes(call));
}

const STATES = FILES.flatMap((f) => parseUnionStates(f.rel, f.src));

// ── 自曝 ────────────────────────────────────────────────────────────────────

if (FILES.length === 0) {
  throw new Error(`[overlay-view-reset] 浮层目录一个源文件都没扫到（${OVERLAY_DIRS.join(', ')}）`);
}
if (!FILES.some((f) => f.rel === 'tray/TrayMenu.tsx')) {
  throw new Error('[overlay-view-reset] 锚点缺失：tray/TrayMenu.tsx —— 被改名/移走了？');
}
if (STATES.length === 0) {
  throw new Error(
    '[overlay-view-reset] 浮层里一个联合字面量 state 都没解析出来 —— 解析器塌了，登记表会恒绿',
  );
}

// ── 登记表 ─────────────────────────────────────────────────────────────────

/**
 * 浮层里全部联合字面量 state。**表外出现新的 → 转红**（新加一个视图态必须显式登记复位口径）。
 *
 * `role`：`view` = 层级视图，必须有 show/focus 复位腿；`transient` = 在飞/静息二元态，不要求复位。
 * `reset`：`wired` = 磁盘上确实有复位腿；`missing` = 没有（逐条写清是待修还是有署名依据）。
 * 两个字段都与磁盘现状**双向**对账：修好了不改登记同样转红。
 */
interface Row {
  file: string;
  name: string;
  role: 'view' | 'transient';
  reset: 'wired' | 'missing';
  note: string;
}

const REGISTRY: readonly Row[] = [
  {
    file: 'tray/TrayMenu.tsx',
    name: 'view',
    role: 'view',
    reset: 'wired',
    note:
      '对齐原型 `proto:4009 openTray()` 的 `trayView(false)`：`onFocus` 里有 `setView(\'main\')`。' +
      '托盘窗常驻不重建（tray.rs 「预建一次（隐藏）」）⇒ 组件永不卸载、state 永不自动复位，' +
      '这条腿是唯一保障 —— 删掉它就回到「上次停在二级视图、下次点图标仍停在二级」。' +
      '切节点 / 切直连处的复位只覆盖「选完就关」那条路径，按返回键或点外部关闭时不经过它们，' +
      '故不能拿那两处顶替本条。**「实际停在哪」仍需真机确认**：窗显隐由 Rust 侧控，' +
      'focus 事件是否每次弹出都触发，这一层看不到。',
  },
  {
    file: 'tray/TrayMenu.tsx',
    name: 'pending',
    role: 'transient',
    reset: 'missing',
    note:
      '不要求复位：`\'start\' | \'stop\' | null` 是本窗发起的在飞启停**方向**，静息值就是 null，' +
      '由启停结果自己收敛（不是层级视图）。登记在此是为了「它哪天变成视图态时门会说话」。',
  },
  {
    file: 'tray/TrayMenu.tsx',
    name: 'updateResult',
    role: 'transient',
    reset: 'wired',
    note:
      '`latest | failed | null` 是单次展开会话里的检查更新回执，不是层级视图；`onFocus` 通过 ' +
      '`setUpdateResult(null)` 明确清掉上一轮结果，避免 warm WebView 下次展开仍展示陈旧结论。',
  },
] as const;

// ── 断言 ────────────────────────────────────────────────────────────────────

describe('守卫自检：解析器正反两向都判得动（防「恒判缺失」让登记表恒真）', () => {
  it('联合字面量 state 解析器在合成样本上命中，且能区分含 null 的', () => {
    const sample = [
      "const [view, setView] = useState<'main' | 'nodes'>('main');",
      "const [pending, setPending] = useState<'start' | 'stop' | null>(null);",
      'const [n, setN] = useState<number>(0);',
      "const [s, setS] = useState<'only'>('only');",
    ].join('\n');
    expect(
      parseUnionStates('x.tsx', sample).map((s) => `${s.name}:${s.members.join('|')}:${s.nullable}`),
    ).toEqual(['view:main|nodes:false', 'pending:start|stop:true']);
  });

  it('复位腿检测器能判「有」——正向对照（否则它只会永远报缺失）', () => {
    const wired = [
      "const [view, setView] = useState<'main' | 'nodes'>('main');",
      'const onFocus = () => {',
      '  applyTheme();',
      "  setView('main');",
      '};',
      "window.addEventListener('focus', onFocus);",
    ].join('\n');
    const st = parseUnionStates('x.tsx', wired)[0];
    expect(hasResetLeg(wired, st), '具名 focus 处理器里的复位腿没被认出来').toBe(true);

    const inline = [
      "const [view, setView] = useState<'main' | 'nodes'>('main');",
      "window.addEventListener('focus', () => { setView('main'); });",
    ].join('\n');
    expect(hasResetLeg(inline, parseUnionStates('x.tsx', inline)[0]), '内联写法没被认出来').toBe(
      true,
    );
  });

  it('复位腿检测器能判「无」——负向对照', () => {
    const missing = [
      "const [view, setView] = useState<'main' | 'nodes'>('main');",
      "const onFocus = () => { applyTheme(); };",
      "window.addEventListener('focus', onFocus);",
      "const switchNode = () => { setView('main'); };", // 不在 focus 腿里 ⇒ 不算
    ].join('\n');
    expect(hasResetLeg(missing, parseUnionStates('x.tsx', missing)[0])).toBe(false);
  });

  it('去注释真的生效（注释里写着 setView(\'main\') 不算实现）', () => {
    const commented = [
      "const [view, setView] = useState<'main' | 'nodes'>('main');",
      "const onFocus = () => { /* 这里应当 setView('main') */ };",
      "window.addEventListener('focus', onFocus);",
    ].join('\n');
    const src = code(commented);
    expect(hasResetLeg(src, parseUnionStates('x.tsx', src)[0])).toBe(false);
  });
});

describe('G14：浮层视图态必须登记，视图态必须有 show/focus 复位腿', () => {
  it('浮层里的联合字面量 state 集合与登记表逐条相等', () => {
    // 变异对照：在 TrayMenu 里加一个 `const [tab, setTab] = useState<'a'|'b'>('a')` → 本条转红。
    const actual = STATES.map((s) => `${s.file}::${s.name}`).sort();
    const registered = REGISTRY.map((r) => `${r.file}::${r.name}`).sort();
    expect(actual, '浮层里多了/少了联合 state —— 见 REGISTRY 头注的登记规则').toEqual(registered);
  });

  it('登记的 role 与磁盘上的类型形态一致（含 null 的不许登记成 view）', () => {
    const wrong = REGISTRY.filter((r) => {
      const st = STATES.find((s) => s.file === r.file && s.name === r.name);
      if (!st) return true;
      return r.role === 'view' ? st.nullable : !st.nullable;
    }).map((r) => `${r.file}::${r.name}(${r.role})`);
    expect(wrong, 'role 与 state 的实际类型对不上').toEqual([]);
  });

  it('登记的 reset 状态与磁盘现状一致（缺失被修好了 ⇒ 也转红，逼登记跟着改）', () => {
    // 变异对照（正向）：把 `setView('main');` 加进 TrayMenu 的 onFocus → 本条转红，
    // 提示「已经修好了，把 REGISTRY 里的 missing 改成 wired」。
    const mismatch = REGISTRY.filter((r) => {
      const st = STATES.find((s) => s.file === r.file && s.name === r.name);
      if (!st) return true;
      const file = FILES.find((f) => f.rel === r.file);
      const wired = !!file && hasResetLeg(file.src, st);
      return wired !== (r.reset === 'wired');
    }).map((r) => `${r.file}::${r.name} 登记=${r.reset}`);
    expect(mismatch, '登记的复位状态与磁盘现状不符 —— 改了实现就把登记一起改').toEqual([]);
  });

  it('缺复位腿的视图态必须逐条写明理由，且不许诉诸「本仓先例」', () => {
    for (const r of REGISTRY.filter((x) => x.reset === 'missing')) {
      expect(r.note.length, `${r.file}::${r.name} 的理由太短`).toBeGreaterThan(30);
      expect(r.note, `${r.file}::${r.name} 的理由是自我循环`).not.toMatch(
        /本仓先例|既有惯例|历来如此/,
      );
      if (r.role === 'view') {
        expect(r.note, `${r.file}::${r.name} 是视图态缺复位，理由须以「待修」起头`).toMatch(/^待修/);
      }
    }
  });

  it('托盘至少有一条 focus 腿（复位腿要挂的地方还在）', () => {
    // 这条挡的是「onFocus 整个被删掉」——那时 hasResetLeg 恒假，上面几条会显得「一切照旧」。
    const tray = FILES.find((f) => f.rel === 'tray/TrayMenu.tsx')!;
    expect(focusHandlerBodies(tray.src).length, '托盘的 window focus 监听没了').toBeGreaterThan(0);
  });
});
