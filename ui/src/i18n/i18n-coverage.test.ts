/**
 * i18n **覆盖面**门 —— 防「某个界面整个漏在 i18n 体系外」。
 *
 * # 为什么必须新开一道门（根因，不是补丁）
 *
 * 仓里此前有两道 i18n 门，但它们都只管**已经接进来的**那部分：
 *  - `locale-parity.test.ts`：五份 locale 之间的键集 / 形态 / 缺口棘轮；
 *  - `styles/text-fit.test.ts`：已登记容器里的文案装不装得下。
 * 两道门的判据面都是 `t('a.b.c')` 这种**已经在用键**的消费点。于是「压根不用键」的界面
 * 对它们**结构性不可见**：
 *  - 托盘浮层自造了 `t(zh: string, en: string)` —— 位置参数双语字面量，只有中/英两态，
 *    ru/fa 用户看到英文。39 个调用点，两道门全绿。
 *  - 更新弹窗连自造 `t()` 都没有，直接把中文写进 `innerHTML` 模板 —— **默认中文、不是回落英文**，
 *    英文用户点开更新提示满屏中文。两道门同样全绿。
 * 产品出 5 个语种，这两个 webview 一个都没接。**没有任何门在管「哪些界面接了 i18n」**，
 * 所以它们能整个漏掉且一路绿到真机。本门管的就是这一层。
 *
 * # 六条判据（G1–G4、G6 是纯源码解析；G5c 例外，用 `renderToStaticMarkup` 真渲染，不需要 jsdom）
 *
 *  G1 **裸 CJK 字面量**：源码里出现中文字符串，且它不是 i18next 的 `defaultValue`、也不是
 *     `console.*` 的实参 ⇒ 计入债务。按**文件逐个精确棘轮**（同 `MISSING_KEY_DEBT` 的口径：
 *     只许降不许升，两个方向都说话）。不在表里的文件一律必须为 **0** ——
 *     于是「**新加一个界面 / 新建一个文件**却把中文直接写死」必然转红。
 *  G2 **自造双参 `t(a, b)`**：`t()` 的首参是字符串字面量但不是点分键 ⇒ 红，**零容忍无棘轮**。
 *     那是「只有两种语言」的签名特征（`t(中文, English)`），正是被修掉的形态。
 *  G3 **每个 webview 入口都追得到 `locales/`**：从 `vite.config.ts` 的多入口出发，解析各入口 HTML
 *     的 `<script src>`，再走**静态 import 图**，断言每个入口都到得了 `i18n/locales/**.json`。
 *     新加一个 webview 入口却没接 i18n ⇒ 图上到不了 ⇒ 红。这是本门的核心一条。
 *  G4 **辅助分区的命名空间归属**：`tray.*` 只许在 `src/tray/` 消费、`updatePopup.*` 只许在
 *     `src/update-popup/` 消费。它们住在 `locales/auxiliary/`，**主窗 i18next 根本没加载**
 *     （见 `i18n/index.ts` 的 resources）⇒ 在主窗写 `t('tray.x')` 会渲染出裸键名，且没有任何
 *     现成的门会红。
 *  G5 **FieldSpec 表里的键真的落地**（2026-08-07 加，与「删掉 `node-spec.ts` 的 151 条 zh/hintZh 缺省」同批）：
 *     a 节点表单（import `ND_SPEC` 真值源）每个 label/hint 键都必须是 en-US 叶子；
 *     b 全仓 FieldSpec 字面量的键**在 en-US 里 或 仍留着 zh 缺省**（接管 `zh` 由必填改可选后丢掉的
 *       那份类型保证）；c 渲染器的每个字段类型都真把 `hint` 渲染进 `.fld-hint`。
 *     根因与射程见下方 G5 段的长注释。
 *  G6 **键集双向对账**（2026-08-07 加）：消费点与 locale 两边互为对方的账。
 *     a 用而未声明（`t('a.b')` 的键不在 locale 里）；b 声明而无消费点（死键）。
 *     G5 是这条的**子集**（只管 FieldSpec 那张表、只管 a 方向）；根因与射程见下方 G6 段。
 *
 * # 读不到就抛，不跳过
 *
 * 所有解析（源码列表 / vite 入口 / HTML script / import 图）解析不到都 `throw`。
 * 「扫到 0 个文件于是 0 个断言全绿」是假门，本仓 `locale-parity` 已为此单列过自检段。
 *
 * # 抓不到什么（射程自曝）
 *
 *  a. **非 CJK 的硬编码文案**：直接写死的英文/俄文字面量。G1 靠 CJK 字符识别「这是给人看的文案」，
 *     纯英文的硬编码与变量名/CSS 类名/协议串无法只靠词法区分。G3 兜住的是入口级的「整个没接」，
 *     入口内部某一条英文硬编码仍可能漏网。
 *  b. **运行期拼出来的键**：`t(`tray.${x}`)` 这类动态键，G4 与 locale-parity 都只覆盖静态字面量。
 *  c. **Rust 侧文案**（2026-07-31 已不再是盲区，但**不由本门守**）：原生对话框 / 托盘原生菜单 /
 *     tooltip / 提权引导 / 应用菜单已接 `src-tauri/src/i18n.rs`，读的是本仓
 *     `locales/auxiliary/` 的 `native.*` 与 `tray.*`，五语齐备。守它的是另外两道门：
 *     `src-tauri/src/i18n.rs` 的 `no_hardcoded_cjk_in_user_facing_native_sinks`
 *     （Rust 词法扫描：用户可见 sink 的实参不得是裸中文字面量）与
 *     `src/contracts/rust-i18n-coverage.test.ts`（两边源码对账）。本门仍是 TS 侧解析器，
 *     够不着 `.rs`；G4 这边只负责「`native.*` 不许被 TS 消费」那一格。
 *  d. **译文质量**：只管「有没有接」和「五语齐不齐」，不管译得对不对。
 */
import { describe, it, expect } from 'vitest';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import ts from 'typescript';
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
// G5 的节点表单一侧**直接取真值源**（导出的纯数据表，对 `field-spec` 只有 `import type`
// ⇒ 不牵进 React，node 环境可直接 import），不靠下面那套 AST 扫描 —— 两条腿的失效模式不同：
// 扫描漏看一个对象字面量会**静默少测**，而 import 拿到的是运行期真的会被渲染的那张表。
import { ND_SPEC, type NodeProto } from '../components/dialogs/node-spec';
import { FieldRenderer, type FieldSpec } from '../components/dialogs/field-spec';

const SRC_DIR = fileURLToPath(new URL('..', import.meta.url));
const UI_DIR = resolve(SRC_DIR, '..');
const rel = (abs: string) => relative(UI_DIR, abs).replace(/\\/g, '/');

/** CJK 统一表意文字 + 扩展 A + 兼容 + CJK 标点 + 全角。假名/谚文不算（本仓不出这两个语种）。 */
const CJK = /[㐀-䶿一-鿿豈-﫿　-〿＀-￯]/;

// ════════════════════════════════════════════════════════════════════════════
// 源码收集
// ════════════════════════════════════════════════════════════════════════════

/** `ui/src/**` + 根层入口（`tray-harness-main.tsx` 之类），排除测试自身。 */
function collectSources(): string[] {
  const acc: string[] = [];
  const walk = (dir: string) => {
    for (const e of readdirSync(dir)) {
      if (e === 'node_modules' || e === 'dist') continue;
      const full = join(dir, e);
      if (statSync(full).isDirectory()) walk(full);
      else if (/\.tsx?$/.test(e) && !/\.(test|spec)\.tsx?$/.test(e)) acc.push(full);
    }
  };
  walk(SRC_DIR);
  for (const e of readdirSync(UI_DIR)) {
    if (/\.tsx?$/.test(e) && !/\.(test|spec|config)\.tsx?$/.test(e)) acc.push(join(UI_DIR, e));
  }
  return acc.sort();
}

const SOURCES = collectSources();

const parse = (file: string) =>
  ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);

/** 点分 i18n 键的形状（与 `locale-parity.test.ts` 的 `extractTKeys` 同口径）。 */
const DOTTED = /^[A-Za-z0-9_]+(\.[A-Za-z0-9_]+)*$/;

interface Hit {
  file: string;
  line: number;
  text: string;
}

/**
 * 扫一个文件里的「裸 CJK 字面量」与「自造双参 t()」。
 *
 * 用 TypeScript 自己的 parser（**已装依赖**，`tsc` 就在跑）而不是正则：注释不是 AST 节点，
 * 于是本仓满篇的中文注释天然不参与判定 —— 换成正则就得手写词法状态机去区分
 * 注释 / 字符串 / 模板 / JSX 文本，那是另一个 bug 源。
 */
function scanSource(file: string, text: string): { bare: Hit[]; selfT: Hit[] } {
  const sf = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const bare: Hit[] = [];
  const selfT: Hit[] = [];
  /** 节点起点 → 豁免理由（`defaultValue` / `console`）。 */
  const exempt = new Map<number, string>();
  const markDeep = (n: ts.Node | undefined, why: string): void => {
    if (!n) return;
    exempt.set(n.getStart(sf), why);
    ts.forEachChild(n, (c) => markDeep(c, why));
  };
  const at = (n: ts.Node) => sf.getLineAndCharacterOfPosition(n.getStart(sf)).line + 1;

  const visit = (n: ts.Node): void => {
    if (ts.isCallExpression(n)) {
      const callee = n.expression;
      const name = ts.isIdentifier(callee)
        ? callee.text
        : ts.isPropertyAccessExpression(callee) && ts.isIdentifier(callee.name)
          ? callee.name.text
          : null;
      // 白名单 ①：console.* 的实参 = 开发者可见的日志，不是出货给用户的界面文案。
      if (
        ts.isPropertyAccessExpression(callee) &&
        ts.isIdentifier(callee.expression) &&
        callee.expression.text === 'console'
      ) {
        for (const a of n.arguments) markDeep(a, 'console');
      }
      if (name === 't' && n.arguments.length >= 1) {
        const a0 = n.arguments[0];
        const lit = ts.isStringLiteral(a0) ? a0.text : null;
        if (lit !== null && DOTTED.test(lit)) {
          // 白名单 ②：i18next 的 `defaultValue`（`t('k','中文')` 与 `t('k',{defaultValue:'中文'})`）。
          // 它有键、进得了 locale-parity 的可寻址性扫描，与「压根没有键」是两回事。
          const a1 = n.arguments[1];
          if (a1 && !ts.isObjectLiteralExpression(a1)) markDeep(a1, 'defaultValue');
          for (const a of n.arguments.slice(1)) {
            if (!ts.isObjectLiteralExpression(a)) continue;
            for (const p of a.properties)
              if (ts.isPropertyAssignment(p) && ts.isIdentifier(p.name) && p.name.text === 'defaultValue')
                markDeep(p.initializer, 'defaultValue');
          }
        } else if (lit !== null) {
          // G2：首参是字符串字面量却不是点分键 ⇒ 自造 t()（`t('中文','English')` 的签名）。
          selfT.push({ file: rel(file), line: at(n), text: lit.slice(0, 40) });
        }
      }
    }
    // 白名单 ③：FieldSpec 表里经 spec 传递的 defaultValue。
    //
    // `field-spec.tsx` 的渲染是 `tr(spec.label, spec.zh)` / `tr(hintKey, hintZh)`（`tr` = `t` 的可选缺省包装）
    // ⇒ `zh` / `hintZh` / `disabledHintZh` 与白名单 ② 的 `t('k','中文')` **是同一语义**，只是分两处写：
    // 键在 `label`/`hint`/`disabledHint`，缺省值在对应的 `*Zh`。② 只认直接形态，看不见这种经表传递的。
    //
    // 为什么不改成「抬基线」：FieldSpec 是本仓弹窗的标准写法（`WgDialog` 12 条、`TsSettingsDialog` 17 条
    // 都是它），每加一个字段就让棘轮红一次 ⇒ 门变成狼来了 ⇒ 最后谁都直接改数字，等于没门。
    //
    // 2026-08-07 `node-spec.ts` 把 151 条 `zh`/`hintZh` 全删了之后，本白名单对**那张表**不再有事可做
    // （它一条 zh 都没有了）——但对另外四个弹窗仍在生效，故不能撤。「删了缺省之后谁来管键漏没漏」
    // 由下方 G5 接手，两者是接力不是重叠。
    //
    // 为什么要求同对象里有键（不是见 `zh:` 就放行）：只按属性名放行的话，任何人给对象加个 `zh:` 就能
    // 藏硬编码文案。要求同一个对象字面量里存在一个**点分 i18n 键**的兄弟属性，才认它是 defaultValue。
    //
    // `disabledHint` / `disabledHintZh` 是 2026-07-31 随「WARP 的 System 开关改成可见但禁用」加进
    // FieldSpec 的一对（禁用时取代 `hint` 的说明）。**登记进本表不是放宽棘轮**：债务数一个没动，
    // 变的是识别器认得这一对属于早已白名单化的「键 + zh 缺省」约定；上面那条「同对象里必须有点分键」
    // 的防滥用要求对它同样成立。
    if (ts.isObjectLiteralExpression(n)) {
      const KEY_PROPS = new Set(['label', 'hint', 'disabledHint']);
      const ZH_PROPS = new Set(['zh', 'hintZh', 'disabledHintZh']);
      const props = n.properties.filter(ts.isPropertyAssignment);
      const hasKey = props.some(
        (p) =>
          ts.isIdentifier(p.name) &&
          KEY_PROPS.has(p.name.text) &&
          ts.isStringLiteral(p.initializer) &&
          DOTTED.test(p.initializer.text)
      );
      if (hasKey) {
        for (const p of props)
          if (ts.isIdentifier(p.name) && ZH_PROPS.has(p.name.text))
            markDeep(p.initializer, 'defaultValue(spec)');
      }
    }
    ts.forEachChild(n, visit);
  };
  visit(sf);

  const walk = (n: ts.Node): void => {
    let raw: string | null = null;
    if (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n)) raw = n.text;
    else if (ts.isJsxText(n)) raw = n.text;
    else if (ts.isTemplateHead(n) || ts.isTemplateMiddle(n) || ts.isTemplateTail(n)) raw = n.text;
    if (raw !== null && CJK.test(raw) && !exempt.has(n.getStart(sf)))
      bare.push({ file: rel(file), line: at(n), text: raw.trim().slice(0, 50) });
    ts.forEachChild(n, walk);
  };
  walk(sf);
  return { bare, selfT };
}

const scanFile = (file: string) => scanSource(file, readFileSync(file, 'utf8'));

const SCAN = SOURCES.map((f) => ({ file: rel(f), ...scanFile(f) }));
const BARE_BY_FILE = new Map<string, Hit[]>();
for (const s of SCAN) if (s.bare.length) BARE_BY_FILE.set(s.file, s.bare);
const ALL_SELF_T = SCAN.flatMap((s) => s.selfT);

// ════════════════════════════════════════════════════════════════════════════
// G1 债务表
// ════════════════════════════════════════════════════════════════════════════

/**
 * 逐条豁免（**不是债务**，是「本来就不该翻译」）。每条都必须写清理由。
 *
 * 与 `BARE_CJK_DEBT` 的区别：债务表里的条目是**欠账，将来要还**；这里的条目**永远不还**。
 * 混在一起会让「还完了没」这个问题失去答案。
 *
 * 表里的文件必须真实存在且真的含裸 CJK（下方有防僵尸自检），否则豁免会烂成一张空头清单。
 */
const I18N_EXEMPT_FILES: Record<string, string> = {
  // 开发用 harness（`harness.html` / `tray-harness.html`）的 mock 数据：假节点名、假订阅名。
  // 它们**不在 vite.config.ts 的 rollupOptions.input 里** ⇒ 不进产物、用户永远看不到；
  // 且内容本就是「像真数据的样例」，翻译它没有意义。
  'harness-fixture.ts': '开发 harness 的 mock 节点/订阅数据，不进产物（不在 vite 多入口里）',
  'tray-harness-main.tsx': '托盘 harness 的 mock 数据与演示用分组名，不进产物',
};

/**
 * 裸 CJK 字面量的**存量**债务 —— 逐文件精确相等，**只许降不许升**。
 *
 * 为什么是「逐文件计数」而不是「逐条白名单」：存量 250 条散在 29 个文件里，逐条登记会变成一张
 * 没人维护的清单；而计数表恰好卡住本门要防的那件事 —— 新文件默认 0（新界面写死中文即红），
 * 老文件涨了也红。两个方向都说话，跟 `MISSING_KEY_DEBT` 同一套棘轮语义。
 *
 * **不在表里 = 必须 0**。托盘（`src/tray/*`）与更新弹窗（`src/update-popup/*`）刻意不在表里：
 * 它们是本次接进 i18n 的两个 webview，任何回潮都必须立刻转红。
 *
 * 表里每条都是**真实的 i18n 缺口**（不是「可以接受」），按文件登记以便逐个还：
 */
const BARE_CJK_DEBT: Record<string, number> = {
  // 节点表单的字段标签/占位/提示，按协议成表（`FIELDS` 一族）—— 全部未走 i18n。最大的一笔。
  'src/components/dialogs/node-spec.ts': 1,
  // 规则域的枚举显示名（条件类型/动作/逻辑词）。
  'src/domain/rules.ts': 33,
  // 暂存层的字段中文名（`FIELD_LABELS` 之类），用于「待应用」条目描述。
  'src/lib/staged-config.ts': 16,
  // 悬浮卡里的规则摘要文案。
  'src/components/hover-cards/RuleHoverCard.tsx': 13,
  // 应用分流页的分组/状态短语。
  'src/components/screens/apppolicy/AppPolicyScreen.tsx': 8,
  // 备份导入的类别名（`CATEGORY_LABELS`）。
  'src/components/dialogs/BackupImportDialog.tsx': 6,
  // 地区规则卡的说明文案。
  'src/components/screens/rules/GeoCard.tsx': 6,
  // 主窗 bootstrap 的**兜底**白屏页：i18next 尚未初始化时的 innerHTML，中英双语并列写死。
  // 与 `ErrorBoundary.tsx` 同理 —— 这一条是**刻意的**，见该文件注释（i18n 挂了它也要能显示）。
  'src/main.tsx': 6,
  // 资源库页的状态短语。
  'src/components/screens/resources/ResourcesScreen.tsx': 5,
  // 错误处理器里的兜底错误描述。
  'src/lib/error-handler.ts': 5,
  // 渲染期异常兜底页：i18n 可能正是挂掉的那部分，故中英双语并列写死（**刻意**，同 main.tsx）。
  'src/components/ErrorBoundary.tsx': 4,
  // 协议编解码的错误描述。
  'src/components/dialogs/proto-codec.ts': 3,
  // 资源引用悬浮卡文案。
  'src/components/hover-cards/ResourceRefsHoverCard.tsx': 3,
  // 显示设置里的语言名（自称名，按惯例不翻译，但仍是裸字面量）。
  'src/components/screens/settings/SettingsDisplay.tsx': 3,
  'src/components/dialogs/RuleDialog.tsx': 0,
  'src/components/screens/connections/ConnectionsScreen.tsx': 2,
  'src/ipc/ipc-client.ts': 2,
  'src/components/screens/nodes/NodeCard.tsx': 1,
  'src/components/screens/settings/SettingsPage.tsx': 1,
  'src/components/screens/settings/useConfig.ts': 1,
  'src/domain/rule-resource-catalog.ts': 1,
  'src/store/use-app-presets-store.ts': 1,
};

// ════════════════════════════════════════════════════════════════════════════
// 自检：解析器真的解析到了东西
// ════════════════════════════════════════════════════════════════════════════

describe('G0 自检：本门不能空转', () => {
  it('扫到足量前端源码', () => {
    expect(SOURCES.length, '源码文件数异常偏低 —— 目录改名 / 收集器失效？').toBeGreaterThan(100);
    expect(SOURCES.some((f) => rel(f) === 'src/tray/TrayMenu.tsx')).toBe(true);
    expect(SOURCES.some((f) => rel(f) === 'src/update-popup/main.ts')).toBe(true);
  });

  it('分类器在合成样本上正反两向都成立（跑的是真扫描器，不是复述）', () => {
    const probe = scanSource(
      'probe.tsx',
      [
        "const a = t('x.y', '默认值甲');", //            豁免：i18next defaultValue（位置参数）
        "const b = t('x.z', { defaultValue: '默认值乙' });", // 豁免：defaultValue（选项对象）
        "console.warn('日志丙');", //                    豁免：开发者可见日志
        "const c = 'ASCII only';", //                    非 CJK，不该命中
        "// 注释里的中文丁", //                          注释不是 AST 节点，不该命中
        "const e = '裸中文戊';", //                      ← 该命中（裸字面量）
        "const f = <div>裸中文己</div>;", //             ← 该命中（JSX 文本）
        "const g = `模板庚 ${x}`;", //                   ← 该命中（模板头）
        "const h = t('中文辛', 'English');", //          ← 该命中（自造双参 t）
      ].join('\n'),
    );
    const bare = probe.bare.map((h) => h.text).sort();
    // 注意 `中文辛` 同时进 bare 与 selfT：自造 t() 的首参**本来就是**一条裸中文字面量，
    // 两条判据各自独立成立（G1 数它、G2 也点名它），刻意不去重 —— 少一层耦合。
    expect(bare, '裸 CJK 分类出错（漏判或误判，逐条见样本注释）').toEqual([
      '中文辛',
      '模板庚',
      '裸中文己',
      '裸中文戊',
    ]);
    expect(probe.selfT.map((h) => h.text), '自造 t() 未被识别').toEqual(['中文辛']);
    // 真实语料上的量级自检：`t('key','中文')` 有 800+ 处，豁免一旦失效债务会涨到四位数。
    const total = [...BARE_BY_FILE.values()].reduce((n, v) => n + v.length, 0);
    expect(total, 'defaultValue 豁免疑似失效（债务暴涨）').toBeLessThan(500);
    expect(total, '扫到 0 条裸 CJK —— CJK 正则失效？').toBeGreaterThan(0);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G1 裸 CJK 棘轮
// ════════════════════════════════════════════════════════════════════════════

describe('G1 裸 CJK 字面量（逐文件精确棘轮，只许降不许升）', () => {
  it('豁免表不得变僵尸（文件仍在、且仍真的含裸 CJK）', () => {
    for (const [file, why] of Object.entries(I18N_EXEMPT_FILES)) {
      expect(why.length, `${file} 的豁免理由不能为空`).toBeGreaterThan(5);
      expect(existsSync(join(UI_DIR, file)), `豁免文件 ${file} 已不存在，请从表里删掉`).toBe(true);
      expect(
        (BARE_BY_FILE.get(file)?.length ?? 0) > 0,
        `豁免文件 ${file} 已无裸 CJK，请从 I18N_EXEMPT_FILES 删掉（否则它在替未来的新增背书）`,
      ).toBe(true);
    }
  });

  it('没有登记债务的文件必须零裸 CJK（新界面写死中文 ⇒ 红）', () => {
    const offenders = [...BARE_BY_FILE.entries()]
      .filter(([f]) => !(f in BARE_CJK_DEBT) && !(f in I18N_EXEMPT_FILES))
      .flatMap(([, hits]) => hits)
      .map((h) => `  ${h.file}:${h.line} ${JSON.stringify(h.text)}`);
    expect(
      offenders.sort(),
      '出现未登记的裸中文字面量。修法：把文案挪进 `i18n/locales/`（辅助 webview 走 `locales/auxiliary/`），' +
        '用 `t(\'ns.key\')` 消费；确属开发者可见（日志/断言）则改走 console。',
    ).toEqual([]);
  });

  it('已登记文件的债务数精确等于基线', () => {
    const drift: string[] = [];
    for (const [file, want] of Object.entries(BARE_CJK_DEBT)) {
      const got = BARE_BY_FILE.get(file)?.length ?? 0;
      if (got !== want)
        drift.push(
          got > want
            ? `  ${file}: 从 ${want} 涨到 ${got} —— 新写的中文请走 i18n`
            : `  ${file}: 已降到 ${got} —— 请把 BARE_CJK_DEBT 里的基线一并调低`,
        );
    }
    expect(drift.sort(), '裸 CJK 债务漂移').toEqual([]);
  });

  it('两个辅助 webview 必须保持零（本次接入 i18n，禁止回潮）', () => {
    for (const dir of ['src/tray/', 'src/update-popup/']) {
      expect(dir in BARE_CJK_DEBT, `${dir} 不得进债务表`).toBe(false);
      const hits = [...BARE_BY_FILE.entries()].filter(([f]) => f.startsWith(dir));
      expect(hits.map(([f]) => f), `${dir} 出现裸中文`).toEqual([]);
    }
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G2 自造双参 t()
// ════════════════════════════════════════════════════════════════════════════

describe('G2 自造 t(a, b) 双参形态（零容忍）', () => {
  it('没有任何 t() 的首参是「非点分键的字符串字面量」', () => {
    expect(
      ALL_SELF_T.map((h) => `  ${h.file}:${h.line} t(${JSON.stringify(h.text)}, …)`).sort(),
      '检出自造 t()。双参位置形态 = 「只有两种语言」的签名（托盘浮层曾如此，ru/fa 用户看英文）。' +
        '修法：把文案入 locale，改 `t(\'ns.key\')`。',
    ).toEqual([]);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G3 webview 入口 → locales 可达性
// ════════════════════════════════════════════════════════════════════════════

/** 从 `vite.config.ts` 读多入口（写死清单就等于新入口默认免检）。 */
function viteEntryHtmls(): string[] {
  const cfg = readFileSync(join(UI_DIR, 'vite.config.ts'), 'utf8');
  const block = cfg.match(/input:\s*\{([\s\S]*?)\}/);
  if (!block) throw new Error('vite.config.ts 里读不到 rollupOptions.input —— 多入口配置被改名？');
  const htmls = [...block[1].matchAll(/__dirname\s*,\s*'([^']+\.html)'/g)].map((m) => m[1]);
  if (htmls.length === 0) throw new Error('vite.config.ts 的 input 里一个 .html 都没解析到');
  return htmls;
}

/** HTML 入口 → 它加载的模块绝对路径。 */
function entryModule(html: string): string {
  const text = readFileSync(join(UI_DIR, html), 'utf8');
  const m = text.match(/<script[^>]*type="module"[^>]*src="([^"]+)"/);
  if (!m) throw new Error(`${html} 里没有 <script type="module" src>`);
  const abs = join(UI_DIR, m[1].replace(/^\//, ''));
  if (!existsSync(abs)) throw new Error(`${html} 的入口模块不存在：${m[1]}`);
  return abs;
}

/** 解析一条 import 说明符（支持 `@/` 别名与相对路径，补 .ts/.tsx/.json 后缀）。 */
function resolveSpec(spec: string, from: string): string | null {
  if (!spec.startsWith('.') && !spec.startsWith('@/')) return null; // 第三方包不跟进
  const base = spec.startsWith('@/') ? join(SRC_DIR, spec.slice(2)) : resolve(dirname(from), spec);
  for (const cand of [base, base + '.ts', base + '.tsx', base + '.json', join(base, 'index.ts'), join(base, 'index.tsx')])
    if (existsSync(cand) && statSync(cand).isFile()) return cand;
  return null;
}

/** 从入口模块出发走静态 import 图（含 `export … from`），返回可达文件集。 */
function importClosure(entry: string): Set<string> {
  const seen = new Set<string>();
  const queue = [entry];
  while (queue.length) {
    const file = queue.pop()!;
    if (seen.has(file) || !/\.tsx?$/.test(file)) {
      seen.add(file);
      continue;
    }
    seen.add(file);
    const sf = parse(file);
    for (const st of sf.statements) {
      const spec =
        (ts.isImportDeclaration(st) || ts.isExportDeclaration(st)) &&
        st.moduleSpecifier &&
        ts.isStringLiteral(st.moduleSpecifier)
          ? st.moduleSpecifier.text
          : null;
      if (!spec) continue;
      const abs = resolveSpec(spec, file);
      if (abs && !seen.has(abs)) queue.push(abs);
    }
  }
  return seen;
}

describe('G3 每个 webview 入口都必须接得上 locales/（新窗漏接 i18n ⇒ 红）', () => {
  const htmls = viteEntryHtmls();

  it('多入口清单解析到了，且含三个已知窗', () => {
    expect(htmls.length, 'vite 入口数异常').toBeGreaterThanOrEqual(3);
    for (const h of ['index.html', 'tray.html', 'update-popup.html']) expect(htmls).toContain(h);
  });

  for (const html of htmls) {
    it(`${html} 的 import 图能到达 i18n/locales/`, () => {
      const closure = importClosure(entryModule(html));
      const locales = [...closure].filter((f) => /i18n[/\\]locales[/\\].*\.json$/.test(f));
      expect(
        locales.map((f) => rel(f)).sort(),
        `${html} 这个 webview 的静态 import 图里没有任何 locale 文件 —— 它的文案要么写死、` +
          '要么自造了一套翻译。修法：文案入 `i18n/locales/`（辅助窗走 `locales/auxiliary/`），' +
          '经 `i18n/index.ts`（主窗）或 `i18n/auxiliary.ts`（辅助窗）消费。',
      ).not.toEqual([]);
      // 五个语种必须都在图里：只引 en-US 就等于「接了但只有一种语言」。
      const langs = new Set(locales.map((f) => f.replace(/^.*[/\\]/, '').replace(/\.json$/, '')));
      expect([...langs].sort(), `${html} 未覆盖全部五语种`).toEqual(['en-US', 'fa', 'ru', 'zh-CN', 'zh-TW']);
    });
  }
});

// ════════════════════════════════════════════════════════════════════════════
// G4 辅助分区命名空间归属
// ════════════════════════════════════════════════════════════════════════════

describe('G4 aux 分区的键只许在对应的消费方里消费', () => {
  /**
   * aux 分区各命名空间的**归属**。三个消费方，两类：
   *  · `tray.*` / `updatePopup.*` —— 辅助 webview，只许在各自目录里 `t()`；
   *  · `native.*` —— **Rust 进程**（`src-tauri/src/i18n.rs`）。TS 侧一处都不许消费，
   *    故 owner 给一个不存在的目录：任何 TS 文件写 `t('native.x')` 都会落进 bad 列表。
   *    （正向那半边 —— Rust 确实在用这些键、且五语齐备 —— 在
   *    `src/contracts/rust-i18n-coverage.test.ts` 与 `i18n.rs` 自己的测试里。）
   */
  const OWNER: Record<string, string> = {
    'tray.': 'src/tray/',
    'updatePopup.': 'src/update-popup/',
    'native.': '<rust:src-tauri/src/i18n.rs>',
  };

  it('没有跨窗消费（在主窗写 t(\'tray.x\') 会渲染出裸键名）', () => {
    const bad: string[] = [];
    for (const file of SOURCES) {
      const sf = parse(file);
      const r = rel(file);
      const text = readFileSync(file, 'utf8');
      for (const [ns, owner] of Object.entries(OWNER)) {
        const re = new RegExp(`\\bt\\(\\s*'(${ns.replace('.', '\\.')}[\\w]+)'`, 'g');
        for (const m of text.matchAll(re))
          if (!r.startsWith(owner)) bad.push(`  ${r}: ${m[1]}（只许在 ${owner} 里用）`);
      }
      void sf;
    }
    expect(bad.sort(), 'aux 命名空间被跨窗消费').toEqual([]);
  });

  it('aux 分区与主分区命名空间互斥，且五语种齐备', () => {
    const auxDir = join(SRC_DIR, 'i18n', 'locales', 'auxiliary');
    const files = readdirSync(auxDir).filter((f) => f.endsWith('.json'));
    expect(files.sort(), 'aux 分区语种不齐').toEqual([
      'en-US.json',
      'fa.json',
      'ru.json',
      'zh-CN.json',
      'zh-TW.json',
    ]);
    const ref = Object.keys(
      JSON.parse(readFileSync(join(auxDir, 'en-US.json'), 'utf8')) as Record<string, unknown>,
    ).sort();
    expect(ref, 'aux 分区的命名空间集合变了').toEqual(['native', 'tray', 'updatePopup']);
    // 新增命名空间必须同时在 OWNER 里表态，否则它对 G4 的跨窗断言**完全不可见**
    // （`native` 就是这么加进来的：不表态的话 TS 侧随手 `t('native.x')` 没有任何门会红）。
    expect(ref, 'aux 有命名空间没在 OWNER 里登记归属').toEqual(Object.keys(OWNER).map((k) => k.slice(0, -1)).sort());
    for (const f of files) {
      const main = JSON.parse(
        readFileSync(join(SRC_DIR, 'i18n', 'locales', f), 'utf8'),
      ) as Record<string, unknown>;
      for (const ns of ref)
        expect(ns in main, `${f}: 命名空间 ${ns} 在主分区也存在 —— 分区必须互斥`).toBe(false);
    }
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G5 FieldSpec 表里的 i18n 键必须真的落地（否则渲染出裸键名）
// ════════════════════════════════════════════════════════════════════════════

/**
 * # 为什么要有这一条（根因）
 *
 * `FieldSpec` 原本靠 **`zh: string` 必填**兜底：键缺失时 `t(key, zh)` 渲染 zh 缺省，界面不会破。
 * 2026-08-07 把 `node-spec.ts` 的 **151 条**缺省（127 条 `zh` + 24 条 `hintZh`；`disabledHintZh` 本来
 * 就是 0 条）全删了（键已五语齐备，留着就是两份真值源且永远看不出哪份在生效），代价是**兜底没了**：
 *
 *  - 实测 i18next 23.16.8：`t('node.field.sni')` 与 `t('node.field.sni', undefined)` 在键缺失时
 *    都返回 **键本身** `"node.field.sni"` ⇒ 漏一条键 = 表单上直接显示一串点分标识符；
 *  - 而 `t(key, '')` 返回 **空串** —— 更隐蔽（一个空的 `.fld-hint`，谁都看不出少了什么），
 *    故 `field-spec.tsx` 里那两处 `hintZh ?? ''` 同批改成了直接传 `hintZh`。
 *
 * 删 `zh` 之前，这条失效面**在 i18n 这一侧**没有门（下面两条），但**不是全仓无门** —— 昨天
 * `243a76c` 把节点弹窗铺进 `text-fit` 时，那里的「双向对差」已经在做键集对账
 * （`text-fit.test.ts`：`expect(used).toEqual(declared)`，`declared` = en-US 里全部 `node.field.*`），
 * 键改名/漏译它会红。立 G5a/G5b 不是因为「一道门都没有」，而是因为那道门有两个结构性问题：
 * ① 它住在**几何门**里，S11 被收窄或重写时这条对账会跟着一起消失；② 它只认 `node.field.` 前缀、
 * 只覆盖 `ND_SPEC`，对另外四个弹窗的 FieldSpec 表完全不可见。
 *
 * i18n 这一侧原先确实看不见，两条原因：
 *  - `locale-parity` 的可寻址性扫描靠正则抓字面量 `t('a.b.c'`，`node-spec.ts` 里**一处 `t(` 都没有**
 *    （键经 spec 表传给 `FieldRenderer`）⇒ 结构上看不见；
 *  - 上面白名单 ③ 只在「同对象里有点分键」时把 `zh` 当 defaultValue 豁免掉 —— 它管的是
 *    「别把中文硬编码算成债务」，`zh` 删光后它无事可做，不会因为键缺失而说话。
 *
 * # 两条腿，失效模式互补
 *
 *  G5a **真值源**：直接 import `ND_SPEC` 遍历。它是运行期真的会被渲染的那张表，
 *     `...F_TRANSPORT` / `tlsAdvFields()` 这类展开与工厂产物全都算数。
 *  G5b **源码扫描**：AST 找出全仓所有 `FieldSpec` 对象字面量（判据 = 同时有 `k:` 与 `t:` 两个
 *     discriminator 属性），要求 `label`/`hint`/`disabledHint` 的键**要么在 en-US 里存在、
 *     要么同对象里还留着对应的 `zh` 兄弟**。
 *
 *     为什么 G5b 的判据是「或」而不是直接要求键存在：`WgDialog` / `WarpDialog` / `TsSettingsDialog` /
 *     `RuleDialog` 的 spec 表建在**组件函数体里**（拿 `draft`/`base` 做 `disabled` 判据），import
 *     不到；而它们今天有 **36 个键根本不在 en-US 里**，全靠 `zh` 兜底渲染中文 —— 那是与
 *     `node.field.*` 同型的存量债（五个语种都看中文），本轮不在射程内。直接要求键存在会一上来
 *     就 36 条红，门立刻变成噪音。
 *
 *     这条「或」同时**接管了 `zh: string` 从必填改成可选后丢掉的那份类型保证**：那四个弹窗少写一个
 *     `zh` 时，编译器不再拦，但 G5b 会拦（键不在 en-US ⇒ 必须有 zh）。
 *
 * # 射程之外
 *
 *  - 只查 **en-US 主分区**。其余语种的齐备性归 `locale-parity`（zh 严格全等、ru/fa 精确棘轮），
 *    缺一条会在那道门红。aux 分区（`tray.*`/`updatePopup.*`）不参与：那两个 webview 没有 FieldSpec 表，
 *    真在主窗写了 `t('tray.x')` 由 G4 管。
 *  - G5b 只认对象字面量。spec 由函数拼装、键由变量传入的写法它看不见 —— 节点表单那份由 G5a 兜住，
 *    其余弹窗目前都是字面量表（208 个键位点全被扫到，见下方自检）。
 */
const EN_MAIN_LEAVES: ReadonlySet<string> = (() => {
  const flat = (o: unknown, p = '', out = new Set<string>()): Set<string> => {
    for (const [k, v] of Object.entries(o as Record<string, unknown>)) {
      const kk = p ? `${p}.${k}` : k;
      if (typeof v === 'string') out.add(kk);
      else if (v && typeof v === 'object') flat(v, kk, out);
    }
    return out;
  };
  const s = flat(JSON.parse(readFileSync(join(SRC_DIR, 'i18n', 'locales', 'en-US.json'), 'utf8')));
  if (s.size < 1000) throw new Error(`en-US 只解析到 ${s.size} 个叶子键 —— locale 读错了？`);
  return s;
})();

/** FieldSpec 的「键属性 → 同对象里的 zh 缺省属性」配对（与 `field-spec.tsx` 的 union 一一对应）。 */
const SPEC_KEY_PROPS: ReadonlyArray<readonly [key: string, zh: string]> = [
  ['label', 'zh'],
  ['hint', 'hintZh'],
  ['disabledHint', 'disabledHintZh'],
];

describe('G5a 节点表单（ND_SPEC 真值源）的每个 i18n 键都必须在 en-US 里存在', () => {
  /** ND_SPEC 摊平 → `键 → 出处`（同一键跨协议复用时留第一处即可，报错定位够用）。 */
  const specKeys = (() => {
    const out = new Map<string, string>();
    let fields = 0;
    for (const proto of Object.keys(ND_SPEC) as NodeProto[])
      for (const section of ['cred', 'adv'] as const)
        for (const f of ND_SPEC[proto][section] as FieldSpec[]) {
          fields++;
          const where = `${proto}.${section}.${f.k}`;
          for (const [kp] of SPEC_KEY_PROPS) {
            const v = (f as unknown as Record<string, unknown>)[kp];
            if (typeof v === 'string' && !out.has(v)) out.set(v, where);
          }
        }
    return { out, fields };
  })();

  it('自检：真的遍历到了整张表（不是空转）', () => {
    // 13 协议 × {cred,adv}，今天 ~200 个字段实例 / 99 个不同的键。数量级掉下来就是表结构变了。
    // 下限跟着本批的 4 条新 hint 一起抬到 99：停在 95 的话，把新加的四条 hint 从 ND_SPEC 里
    // 摘掉本门仍全绿，只有 text-fit 那道几何门会红 —— 而本段的立意正是「别把键集对账托付给几何门」。
    expect(specKeys.fields, 'ND_SPEC 字段实例数异常偏低 —— 表结构变了？').toBeGreaterThan(100);
    expect(specKeys.out.size, 'ND_SPEC 收集到的 i18n 键数异常偏低').toBeGreaterThanOrEqual(99);
  });

  it('每个 label / hint / disabledHint 键都是 en-US 的叶子键（缺一条 ⇒ 表单渲染裸键名）', () => {
    const missing = [...specKeys.out]
      .filter(([key]) => !EN_MAIN_LEAVES.has(key))
      .map(([key, where]) => `  ${key}（${where}）`)
      .sort();
    expect(
      missing,
      'ND_SPEC 用了 en-US 里不存在的键。i18next 对缺失键返回**键本身** ⇒ 用户会看到 `node.field.xxx`。' +
        '修法：把文案补进 `i18n/locales/*.json`（五语同批，否则 locale-parity 的 ru/fa 棘轮也会红）。',
    ).toEqual([]);
  });
});

describe('G5b 全仓 FieldSpec 字面量：键在 en-US 里，或仍留着 zh 缺省', () => {
  /** 判据 = 同时有 `k:` 与 `t:`（FieldSpec 的两个 discriminator），把 select 选项 / 导航项之类排除掉。 */
  const sites = (() => {
    const acc: { file: string; line: number; prop: string; key: string; hasZh: boolean }[] = [];
    for (const file of SOURCES) {
      const sf = parse(file);
      const visit = (n: ts.Node): void => {
        if (ts.isObjectLiteralExpression(n)) {
          const named = new Map<string, ts.PropertyAssignment>();
          for (const p of n.properties)
            if (ts.isPropertyAssignment(p) && ts.isIdentifier(p.name)) named.set(p.name.text, p);
          if (named.has('k') && named.has('t'))
            for (const [kp, zhp] of SPEC_KEY_PROPS) {
              const p = named.get(kp);
              if (!p || !ts.isStringLiteral(p.initializer) || !DOTTED.test(p.initializer.text)) continue;
              acc.push({
                file: rel(file),
                line: sf.getLineAndCharacterOfPosition(p.getStart(sf)).line + 1,
                prop: kp,
                key: p.initializer.text,
                hasZh: named.has(zhp),
              });
            }
        }
        ts.forEachChild(n, visit);
      };
      visit(sf);
    }
    return acc;
  })();

  it('自检：扫到了足量键位点，且节点表单那份**没有**任何 zh 缺省（第二腿已落地）', () => {
    expect(sites.length, 'FieldSpec 键位点异常偏低 —— 判据（k + t）失效？').toBeGreaterThan(150);
    const nodeSpec = sites.filter((s) => s.file === 'src/components/dialogs/node-spec.ts');
    expect(nodeSpec.length, 'node-spec.ts 一个键位点都没扫到').toBeGreaterThanOrEqual(99);
    // 这一条是「删 zh」那件事本身的牙：任何人往 node-spec.ts 里补回一个 zh 缺省都会红。
    expect(
      nodeSpec.filter((s) => s.hasZh).map((s) => `  ${s.file}:${s.line} ${s.prop}=${s.key}`).sort(),
      'node-spec.ts 又出现了 zh 缺省 —— 该表的文案真值源只有 locale 一处',
    ).toEqual([]);
  });

  it('没有「键不在 en-US 且没有 zh 缺省」的字段（那就是渲染裸键名）', () => {
    const bad = sites
      .filter((s) => !EN_MAIN_LEAVES.has(s.key) && !s.hasZh)
      .map((s) => `  ${s.file}:${s.line} ${s.prop}='${s.key}'`)
      .sort();
    expect(
      bad,
      '这些 FieldSpec 既没有 en-US 译文、也没有 zh 缺省 ⇒ 界面上会出现点分键名。' +
        '修法：把键补进 `i18n/locales/*.json`（首选，五语同批），或暂时保留 zh 缺省。',
    ).toEqual([]);
  });
});

/**
 * G5c 说明行**真的被渲染出来**（每个字段类型各一份）。
 *
 * # 为什么这条门必须存在
 *
 * 2026-08-07 把 `hint` 从 select/switch 两支提到了 `FieldBase`（text/number/textarea 也能有说明），
 * 于是 `FieldRenderer` 的**五个分支都得渲染它**。漏掉某一支的下场是「locale 里躺着一条译文、
 * text-fit 还在按它量宽度、用户却永远看不到」——**类型不红、build 不红**，而本仓 vitest 是
 * `environment:'node'`，静态扫描也看不出一个 JSX 分支少写了 `{hintEl}`。这正是本文件开篇讲的
 * 那类缺陷（文案漏在用户可见路径之外），只是漏点从「整个界面」缩到了「渲染器的一支」。
 *
 * # 四支真渲染，`select` 一支退回源码判据（**不是偷懒，是环境边界**）
 *
 * `renderToStaticMarkup` 只要 React 本身，不需要 jsdom（既有先例：
 * `components/dialogs/field-switch-disabled.test.tsx`）。text/number/textarea/switch 四支这么测。
 * **`select` 测不了**：它渲染 `<Csel>`，而 `Csel.tsx:335` 无上下文时走
 * `createPortal(menuNode, document.body)` —— node 环境下 `document is not defined`，整棵树炸在渲染期。
 * 那一支改用源码判据：断言 select 分支的 JSX 里确实有 `{hintEl}`（与另外三支同一个常量）。
 * 弱一档，但方向正确：漏写仍会红。用 `createElement` 而不是 JSX 单纯因为本文件是 `.ts`。
 *
 * # 断言打在**键**上而不是译文上
 *
 * 未初始化 i18n 时 `t(key)` 返回**键本身**（实测 23.16.8，也是 G5a/G5b 那两条门的前提），
 * 所以「键出现在 HTML 里」就等价于「这一支把 hint 渲染出去了」，不必在测试里搭一套 i18n 资源。
 */
describe('G5c FieldRenderer 每个字段类型都把 hint 渲染进 `.fld-hint`', () => {
  const HINT_KEY = 'probe.hint.key';
  const FIELD_SPEC_SRC = readFileSync(join(SRC_DIR, 'components', 'dialogs', 'field-spec.tsx'), 'utf8');
  const html = (over: Record<string, unknown>) =>
    renderToStaticMarkup(
      createElement(FieldRenderer, {
        spec: { k: 'probe', label: 'probe.label.key', ...over } as FieldSpec,
        value: undefined,
        onChange: () => {},
      }),
    );

  /** 可直接渲染的四支。`select` 见上方注释（Csel 要 DOM），单列在下面一条。 */
  const RENDERABLE: Record<string, Record<string, unknown>> = {
    text: { t: 'text' },
    number: { t: 'number' },
    textarea: { t: 'textarea' },
    switch: { t: 'switch' },
  };
  const DOM_ONLY = ['select'];

  it('自检：登记的字段类型 == FieldSpec 的全部 discriminant（漏登一支 = 这道门对它免检）', () => {
    // 真值取自 `field-spec.tsx` 源码里 union 定义处的 `t: '<kind>'` 字面量，不是手抄。
    const block = FIELD_SPEC_SRC.match(/export type FieldSpec =([\s\S]*?)\n\n/);
    if (!block) throw new Error('field-spec.tsx 里读不到 `export type FieldSpec = …` —— union 改写法了？');
    const kinds = [...block[1].matchAll(/\bt:\s*'([a-z]+)'/g)].map((m) => m[1]).sort();
    expect(kinds, 'FieldSpec 的 union 分支与本门登记的类型对不上').toEqual(
      [...Object.keys(RENDERABLE), ...DOM_ONLY].sort(),
    );
  });

  for (const [kind, over] of Object.entries(RENDERABLE)) {
    it(`${kind}：给了 hint 就必须渲染出来；没给就不许凭空长出 .fld-hint`, () => {
      const withHint = html({ ...over, hint: HINT_KEY });
      // 标签同理一并钉住：`tr(spec.label, spec.zh)` 若哪天被写成 `t(key, zh ?? '')`，
      // 没有 zh 缺省的字段（= 今天整张 ND_SPEC）会渲染出**空标签**而不是键名，同样一眼看不出。
      expect(withHint, `${kind} 分支没把 label 渲染出来`).toContain('probe.label.key');
      expect(withHint, `${kind} 分支填了 hint 却没渲染 —— 那条译文永远到不了用户眼前`).toContain(
        HINT_KEY,
      );
      expect(withHint, `${kind} 的 hint 没有落在 .fld-hint 里（样式对不上）`).toContain('fld-hint');
      // 阴性对照：没有 hint 时不得出现 `.fld-hint`，否则上面那条断言可能只是命中了别处的固定文本。
      expect(html(over), `${kind} 没给 hint 却渲染了 .fld-hint`).not.toContain('fld-hint');
    });
  }

  it(`select：渲染不了（Csel 要 DOM），改判源码里那一支确实挂了 {hintEl}`, () => {
    // 先钉住「渲染不了」这个前提本身 —— 哪天 Csel 不再无条件用 document 了，这条会红，
    // 提示把 select 挪回上面的真渲染那一组（否则本条会永远停在弱判据上，没人再回来看）。
    expect(() => html({ t: 'select', options: [['a', 'A']], hint: HINT_KEY })).toThrow(/document/);
    const branch = FIELD_SPEC_SRC.match(/if \(spec\.t === 'select'\) \{[\s\S]*?\n  \}/);
    if (!branch) throw new Error('field-spec.tsx 里读不到 select 分支 —— 渲染器改写法了？');
    expect(branch[0], 'select 分支没挂 {hintEl}（填了 hint 也不会显示）').toContain('{hintEl}');

    // 「挂了 `{hintEl}`」只挡得住**字面删掉**那一种改法。另一种同后果的改法是把 `hintEl` 自己的
    // 生成条件收窄（`spec.hint && spec.t !== 'select'` 之类）—— 四个真渲染分支照常绿（它们仍拿得到
    // hintEl）、本条的 `toContain` 也照常绿（JSX 还在），而 ND_SPEC 里挂在 select 上的 hint 键会在
    // 5 个语种下**静默消失**。故把 `hintEl` 的定义式一起钉死：它只许看 `spec.hint`，不许分字段类型。
    const def = FIELD_SPEC_SRC.match(/const hintEl = ([^;]+);/);
    if (!def) throw new Error('field-spec.tsx 里读不到 `const hintEl = …` —— 共享 hint 节点改写法了？');
    expect(
      def[1],
      'hintEl 的生成条件里出现了 `spec.t` —— 按字段类型分档会让某一支的 hint 静默消失，' +
        '而真渲染的四支与上面的 toContain 都不会红',
    ).not.toMatch(/spec\.t\b/);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G6 键集双向对账（消费点 ↔ locale）
// ════════════════════════════════════════════════════════════════════════════

/**
 * # 为什么这条必须存在（根因）
 *
 * 仓里此前的三道 i18n 门各管一段，**两个方向的键集对账都没人管**：
 *  - `locale-parity`：五份 locale **彼此之间**齐不齐、形态一致不一致、消费点会不会被字符串父级挡住。
 *    注意最后那条的失效形态：`blockedBy()` 对**纯缺键**显式 `return null`（注释写着「不是形态问题」）
 *    ⇒ 一个压根不存在的键在那道门里是**合法**的，五个语种一起没有也照样全绿。
 *  - `i18n-coverage` G1/G2：管「文案有没有走 i18n」，不管键存不存在。
 *  - `i18n-coverage` G5：管键存不存在，但**只管 FieldSpec 表**那一张、且只管 a 方向。
 *
 * 于是两类缺陷可以一路绿到真机：
 *
 *  **a 用而未声明**。i18next 23.16.8 的解析顺序是 `locale` → `fallbackLng` → `defaultValue`：
 *   · `t('home.relJustNow')` 无缺省 ⇒ 返回**键本身**，用户在首页看到一串 `home.relJustNow`；
 *   · `t('resources.update', '更新')` 有缺省 ⇒ 五个语种**全部**渲染那句中文（fallbackLng 也没有该键，
 *     轮到 defaultValue）。这与 `common.optional` 那个缺陷（ru/fa 看中文）同型，只是范围更大 ——
 *     那条至少 en-US 有键，这里连 en-US 都没有。
 *   建门当天实测：无缺省 6 处、有缺省 34 处。
 *
 *  **b 声明而无消费点**。移植期整批搬进来的译文，界面没接（或接了后来又删了消费点）。它们不会
 *   报错、不会显示、也不会被任何门看见，只是让 locale 越长越大、让「这句文案在哪用」查无对证。
 *   `localImport.*` 是最纯的一例：29 键 × 5 语，**零消费点**，而同一个弹窗另起了一套 `import.*`。
 *   同一批里还查出四处**改名没跟**（locale 里躺着旧键名的真译文、消费点写的是新名）——
 *   这类缺陷天然同时出现在 a 与 b 两侧，单看一侧只会当成「缺一条译文」而去补一条重复的。
 *
 * # 「消费点」的判据：源码里出现该键的**字面量**
 *
 * 不只认 `t('k')` —— 本仓有大量**表驱动**消费（`labelKey: 'sidebar.home'`、FieldSpec 的
 * `label: 'node.field.sni'`、`NAV_DEF` 一族），只认 `t(` 会把它们全判成死键。故判据放宽成
 * 「该键作为字符串字面量在非测试源码里出现过」，代价是**理论上**可能有别的字面量恰好等于某个
 * 点分键（实测无）。三条补充口径：
 *  · 动态键（模板串拼出来的键）→ 该模板头前缀下**整棵子树**算被消费（前缀从模板头解析，不写死清单）；
 *  · `native.*` 与部分 `tray.*` 的消费方是 **Rust 进程**，读 `src-tauri/src/i18n.rs` 的 `mod key`
 *    常量表（与 `contracts/rust-i18n-coverage.test.ts` 同一份真值源，那边守的是反向）；
 *  · 注释不参与 —— 用的是 TS parser，注释不是 AST 节点。本仓注释里逐字引用键名的地方极多
 *    （含本文件），换成正则会把它们全算成消费点，门就废了。
 *
 * # 为什么 a 分两档、b 走棘轮
 *
 * a 的**无缺省**那半是「屏幕上直接显示点分标识符」，没有任何可辩护的中间态 ⇒ 零容忍。
 * **有缺省**那半是存量（建门当天 34 处），逐键登记：每条都是真缺口，登记只是为了让它只降不升。
 * b 今天有 1000+ 条（上游 整份 locale 搬进来、Polaris 只接了一半），逐条登记不现实，
 * 故按**顶层命名空间精确计数**棘轮 —— 与 `BARE_CJK_DEBT` 同一套语义：新增一个死键会红，
 * 清掉一批也会红（提示把基线调低）。
 *
 * # 射程之外
 *
 *  · 运行期拼出来、且模板头不含前缀的键（`t(keyFromTable)`）：a 看不见它（不是字面量），
 *    b 那边它消费的键若在别处也没出现过则会被误判成死键 —— 届时按同类办法把前缀补上，或让基线说话。
 *  · 译文对不对、五语齐不齐：分别归「人」和 `locale-parity`。
 */
const RUST_I18N_SRC = join(UI_DIR, '..', 'src-tauri', 'src', 'i18n.rs');

/** en-US 主分区 + aux 分区的全部叶子键 = 「已声明」的真值。 */
const DECLARED_LEAVES: ReadonlySet<string> = (() => {
  const flat = (o: unknown, p = '', out: Set<string> = new Set()): Set<string> => {
    for (const [k, v] of Object.entries(o as Record<string, unknown>)) {
      const kk = p ? `${p}.${k}` : k;
      if (typeof v === 'string') out.add(kk);
      else if (v && typeof v === 'object') flat(v, kk, out);
    }
    return out;
  };
  const out = new Set<string>(EN_MAIN_LEAVES);
  const auxFile = join(SRC_DIR, 'i18n', 'locales', 'auxiliary', 'en-US.json');
  flat(JSON.parse(readFileSync(auxFile, 'utf8')), '', out);
  if (out.size <= EN_MAIN_LEAVES.size) throw new Error('aux 分区一个键都没并进来 —— 路径变了？');
  return out;
})();

interface TCall {
  file: string;
  line: number;
  key: string;
  /** 有 defaultValue（位置参数或 `{defaultValue}`）时为那句缺省文案，否则 `null`。 */
  dflt: string | null;
}

/** 全仓 `t(...)` / `tr(...)` 的**字面量键**调用点 + 字符串字面量池 + 动态键前缀。 */
const G6_SCAN = (() => {
  const calls: TCall[] = [];
  const literals = new Set<string>();
  const prefixes = new Set<string>();
  for (const file of SOURCES) {
    const sf = parse(file);
    const visit = (n: ts.Node): void => {
      if (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n)) literals.add(n.text);
      // 模板串拼出来的键 → 模板头即前缀。要求以 `.` 收尾且非空，否则空头模板也会进来。
      if (ts.isTemplateExpression(n) && n.head.text.endsWith('.') && n.head.text.length > 1) {
        prefixes.add(n.head.text);
      }
      if (ts.isCallExpression(n)) {
        const c = n.expression;
        const name = ts.isIdentifier(c)
          ? c.text
          : ts.isPropertyAccessExpression(c) && ts.isIdentifier(c.name)
            ? c.name.text
            : null;
        if ((name === 't' || name === 'tr') && n.arguments.length >= 1) {
          const a0 = n.arguments[0];
          if (ts.isStringLiteral(a0) && DOTTED.test(a0.text)) {
            let dflt: string | null = null;
            const a1 = n.arguments[1];
            if (a1 && ts.isStringLiteral(a1)) dflt = a1.text;
            else if (a1 && ts.isObjectLiteralExpression(a1)) {
              for (const p of a1.properties) {
                if (
                  ts.isPropertyAssignment(p) &&
                  ts.isIdentifier(p.name) &&
                  p.name.text === 'defaultValue' &&
                  ts.isStringLiteral(p.initializer)
                ) {
                  dflt = p.initializer.text;
                }
              }
            }
            calls.push({
              file: rel(file),
              line: sf.getLineAndCharacterOfPosition(n.getStart(sf)).line + 1,
              key: a0.text,
              dflt,
            });
          }
        }
      }
      ts.forEachChild(n, visit);
    };
    visit(sf);
  }
  // Rust 侧消费的键（`native.*` 与它与浮层共用的那批 `tray.*`）。读不到就抛。
  const rustSrc = readFileSync(RUST_I18N_SRC, 'utf8');
  const rustKeys = [...rustSrc.matchAll(/pub const \w+: &str = "([\w.]+)";/g)].map((m) => m[1]);
  if (rustKeys.length < 30) {
    throw new Error(`src-tauri/src/i18n.rs 只解析到 ${rustKeys.length} 个键常量 —— 写法变了？`);
  }
  return { calls, literals, prefixes, rustKeys: new Set(rustKeys) };
})();

/**
 * G6a **有 defaultValue** 的存量缺口 —— 逐键登记，只许降不许升。
 *
 * 每条都是真缺口：这些键一个语种都没有，五语用户看到的全是代码里那句中文缺省。
 * 还账方式是把文案补进 `i18n/locales/*.json`（五语同批）后从本表删掉。
 * **无缺省那一档不在这里**：那是零容忍档（屏幕上显示点分键名），见下方断言。
 */
const UNDECLARED_KEY_DEBT: ReadonlySet<string> = new Set<string>([
  // 2026-08-07：**已清零**。建门当天登记了 24 条（`resources.update` / `rules.modeNote` /
  // `nodes.warpSlotTaken` …），随即五语同批补齐并从这里删掉 —— 于是这一档和上面那档一样是
  // 零容忍。再往里加条目前先想清楚：为什么这条键不能现在就补进 locale。
]);

/**
 * G6b 死键的**存量**基线，按顶层命名空间精确计数（同 `BARE_CJK_DEBT` 的棘轮语义）。
 *
 * 数字的来历：上游 那份 locale 是整批搬进来的，而 Polaris 只接了其中一部分屏。
 * 不在表里的命名空间一律必须为 **0** —— 于是「新加一批译文却没人用」必然转红。
 */
const DEAD_KEY_DEBT: Record<string, number> = {
  // 2026-08-07：**已清零，本档改为零容忍**（与 G6a 两档一致）。
  //
  // 建门当天存量 1107 条，分两步还完：先把 `servers` / `ruleResources` / `apiToast` 三段
  // 名不副实的命名空间整段收尾（−515，活键搬回 `nodes.*` / `resources.*` / `sub.*`），
  // 再把剩余 592 条逐条删掉。判据全程是本门那一套（字面量 / 模板前缀 / Rust `mod key`）。
  //
  // 删之前单独核过**全部 57 处非字面量 `t()` 调用点**：6 个键构造函数都是模板串
  // （`rules.types.${id}.name` 一族，前缀已被本门覆盖），其余从字面量表取键，
  // **没有任何键是 `+` 拼出来的** ⇒ 本门的射程等于真实消费面，删除集不含活键。
  //
  // 往这张表里加条目 = 承认「新增了没人用的译文」。先想清楚为什么不能当场接上或不写。
};

describe('G6 键集双向对账（消费点 ↔ locale）', () => {
  const missing = G6_SCAN.calls.filter((c) => !DECLARED_LEAVES.has(c.key));

  it('自检：扫到了足量 t() 调用点与字面量（防「扫到 0 条于是全绿」）', () => {
    expect(G6_SCAN.calls.length, 't() 调用点异常偏低 —— 识别器失效？').toBeGreaterThan(800);
    expect(G6_SCAN.literals.size, '字符串字面量池异常偏低').toBeGreaterThan(2000);
    expect(DECLARED_LEAVES.size, '已声明键数异常偏低').toBeGreaterThan(1000);
    // 动态键前缀至少要认出既有的那几条，否则 G6b 会把整棵 `rules.types.*` 判成死键。
    expect(G6_SCAN.prefixes.has('rules.types.'), '动态键前缀识别失效').toBe(true);
  });

  it('G6a-1 没有「用了却没声明、且没有 defaultValue」的键（那会把点分键名画到屏幕上）', () => {
    const bad = missing
      .filter((c) => c.dflt === null)
      .map((c) => `  ${c.file}:${c.line} ${c.key}`)
      .sort();
    expect(
      bad,
      'i18next 对缺失键返回**键本身** ⇒ 用户看到一串点分标识符。' +
        '修法：把文案补进 `i18n/locales/*.json`（五语同批）；若别处已有同义键，改指过去而不是新造。',
    ).toEqual([]);
  });

  it('G6a-2 「用了却没声明、但有 defaultValue」精确等于已登记的存量（只许降不许升）', () => {
    const got = [...new Set(missing.filter((c) => c.dflt !== null).map((c) => c.key))].sort();
    const added = got.filter((k) => !UNDECLARED_KEY_DEBT.has(k));
    const fixed = [...UNDECLARED_KEY_DEBT].filter((k) => !got.includes(k)).sort();
    expect(
      { added, fixed },
      '新增的键没有译文 ⇒ 五个语种都渲染代码里那句中文缺省（与 `common.optional` 同型）。' +
        '补进 locale 后请把 UNDECLARED_KEY_DEBT 里的对应条目删掉。',
    ).toEqual({ added: [], fixed: [] });
  });

  it('G6b 死键（声明了但全仓无消费点）逐命名空间精确等于基线', () => {
    const consumed = (k: string) =>
      G6_SCAN.literals.has(k) ||
      G6_SCAN.rustKeys.has(k) ||
      [...G6_SCAN.prefixes].some((p) => k.startsWith(p));
    const byNs = new Map<string, string[]>();
    for (const k of DECLARED_LEAVES) {
      if (consumed(k)) continue;
      const ns = k.split('.')[0];
      byNs.set(ns, [...(byNs.get(ns) ?? []), k]);
    }
    const drift: string[] = [];
    for (const [ns, keys] of byNs) {
      const want = DEAD_KEY_DEBT[ns] ?? 0;
      if (keys.length === want) continue;
      drift.push(
        keys.length > want
          ? `  ${ns}: 死键从 ${want} 涨到 ${keys.length} —— 新增了没人消费的译文，或某个消费点被删了。` +
            `样例：${keys.sort().slice(0, 6).join(' ')}`
          : `  ${ns}: 死键已降到 ${keys.length} —— 请把 DEAD_KEY_DEBT.${ns} 一并调低`,
      );
    }
    for (const ns of Object.keys(DEAD_KEY_DEBT)) {
      if (!byNs.has(ns)) drift.push(`  ${ns}: 已无死键 —— 请从 DEAD_KEY_DEBT 删掉该条`);
    }
    expect(drift.sort(), '死键漂移（声明了但没有任何消费点的译文）').toEqual([]);
  });
});
