/**
 * 样式不变量门 —— 守 2026-07-21 三处真机 UI 缺陷的**根因**，而非它们当时的具体取值。
 *
 * 为什么是「读源文件断言」而非渲染断言：vitest 环境是 `node`（vite.config.ts `test.environment`），
 * 没有 jsdom / 没有 CSSOM，样式的最终层叠结果在本层根本不可观测。视觉效果本就要真机看（mac vibrancy、
 * 深色 toast 已标为待真机验）。但**导致缺陷的结构**是纯文本可断言的，且正是最容易复发的那一层：
 *   1. toast 底距被重新钉成裸常数（原缺陷：`bottom:96` 里含 36px 死账 —— pending bar 早已不吸底）；
 *   2. 两份重复的 `.toast` 只改了一份（本仓反复出现的坑：prototype.css / components.css 各存一套，
 *      谁最后 @import 谁生效，只改一份 = 没改）；
 *   3. 开/关文案不对称（开态带语义说明、关态只有裸标题）。
 * 三条都能被「改坏生产代码 → 测试转红」验证（变异验证已跑，见交接说明）。
 *
 * 本门**不**断言具体像素/色值：32px、28px、surface-2 都可以合理演进，钉死只会制造假阻力。
 * 断言的是关系：bottom 必须**从状态栏高度推导**、中性暗色覆盖必须**两份都在**、开关文案必须**同句式**。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

/** 去掉 CSS/JS 注释，避免注释里引用的示例代码被当成真实声明命中。 */
const stripComments = (src: string) =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');

/** 两份逐字重复的 `.toast` 定义所在文件 —— 缺陷 3「只改一份等于没改」的射程。 */
const TOAST_DUPLICATE_FILES = ['./prototype.css', './components.css'] as const;

// ── WCAG 算术（8bit 取整后再算，与浏览器渲染同一套；正确性由各对比度门的 ⓪ 自校）────────────
// 放模块级供多道对比度门共用（浅色 --err / 浅色 warn·ok·dn / 深色 .btn.danger），别再复制一份。
type RGB = [number, number, number];
/** `H S% L%` 三元组 → 8bit sRGB。 */
const hslToRgb = (triple: string): RGB => {
  const [h, s, l] = triple.split(/\s+/).map((x) => parseFloat(x));
  const S = s / 100;
  const L = l / 100;
  const a = S * Math.min(L, 1 - L);
  const f = (n: number) => {
    const k = (n + h / 30) % 12;
    return Math.round((L - a * Math.max(-1, Math.min(k - 3, Math.min(9 - k, 1)))) * 255);
  };
  return [f(0), f(8), f(4)];
};
/** fg 以 alpha 叠在 bg 上（浏览器的 source-over）。 */
const over = (fg: RGB, alpha: number, bg: RGB): RGB =>
  fg.map((v, i) => Math.round(v * alpha + bg[i] * (1 - alpha))) as RGB;
const lum = (c: RGB) => {
  const ch = c.map((v) => {
    const x = v / 255;
    return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * ch[0] + 0.7152 * ch[1] + 0.0722 * ch[2];
};
/** WCAG 对比度，保留两位（与实测报数同精度）。 */
const contrast = (a: RGB, b: RGB) => {
  const [x, y] = [lum(a), lum(b)];
  return Math.round(((Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05)) * 100) / 100;
};
/** CIE L*（D65）—— 注释里报的「明度」用它，不是 HSL 的 L。 */
const lstar = (c: RGB) => {
  const Y = lum(c);
  return Math.round((Y <= 216 / 24389 ? Y * (24389 / 27) : Math.cbrt(Y) * 116 - 16) * 100) / 100;
};

/**
 * 扁平规则。选择器只取**最后一个 `;` 之后**的部分 —— `@import '…';` / `@tailwind x;` 这类
 * 以分号收尾的 at-rule 会被前面的 `[^{}]+` 一并吞进选择器捕获，不切掉就永远匹配不上。
 * （`@media` 里的内层规则也会被拆出来，按选择器认的门不受影响。）
 */
const flat = (css: string) =>
  [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({
    sel: m[1].split(';').pop()!.trim().replace(/\s+/g, ' '),
    body: m[2],
  }));
const readVar = (body: string, name: string) =>
  body.match(new RegExp(`${name}\\s*:\\s*([^;]+);`))?.[1].replace(/\s+/g, ' ').trim();

describe('toast 底距：必须由状态栏高度推导，不得是裸常数', () => {
  const indexCss = stripComments(read('./index.css'));

  it('index.css 定义 --statusbar-h，且 .statusbar 的 height 就用它（单一真值，非平行常数）', () => {
    expect(indexCss).toMatch(/--statusbar-h\s*:/);
    // 必须真的接管 .statusbar 的 height —— 否则 var 只是个没人用的孤儿常数，
    // 状态栏改高时 toast 依旧不跟随（缺陷原样复发）。
    expect(indexCss).toMatch(/\.statusbar\s*\{[^}]*height\s*:\s*var\(\s*--statusbar-h\s*\)/);
  });

  it('#toast-stack 的 bottom 是 --statusbar-h 的 calc 推导，而非写死的 px', () => {
    const rule = indexCss.match(/#toast-stack\s*\{([^}]*)\}/);
    expect(rule, '#toast-stack 规则不存在 —— bottom 又回内联了？').not.toBeNull();
    const body = rule![1];
    expect(body).toMatch(/bottom\s*:\s*calc\([^)]*var\(\s*--statusbar-h\s*\)/);
    // 裸数值兜底：`bottom:60px` 这种「算好了写死」同样是缺陷复发（状态栏一改就再次错位）。
    expect(body).not.toMatch(/bottom\s*:\s*\d/);
  });

  it('Toaster.tsx 内联 style 不得再设 bottom（内联优先级会压掉上面的推导）', () => {
    const toaster = stripComments(read('../components/layout/Toaster.tsx'));
    expect(toaster).not.toMatch(/\bbottom\s*:/);
  });
});

describe('toast 中性态暗色覆盖：两份重复定义必须都改', () => {
  for (const file of TOAST_DUPLICATE_FILES) {
    const css = stripComments(read(file));

    it(`${file} 仍持有反相底的基础 .toast（前提校验：改动没跑偏到别处）`, () => {
      expect(css).toMatch(/^\.toast\s*\{[^}]*background\s*:\s*hsl\(var\(--fg\)\)/m);
    });

    // 暗色有两条腿：系统偏好（@media prefers-color-scheme）与显式 data-theme="dark"。
    // 本仓 uiTheme 支持 'system' / 'dark' 显式二选一，少任何一条腿都会漏掉一半用户。
    it(`${file} 的 @media prefers-color-scheme:dark 腿有中性 .toast 覆盖`, () => {
      // 文件里有多个 dark media 块（logic-toggle / connect-btn / …），只取到第一个会误判 ——
      // 断言「**存在某个** dark 块含中性 toast 覆盖」。
      const blocks = [
        ...css.matchAll(/@media\s*\(prefers-color-scheme\s*:\s*dark\)\s*\{([\s\S]*?)\n\}/g),
      ].map((m) => m[1]);
      expect(blocks.length, 'dark media 块一个都没有').toBeGreaterThan(0);
      const neutral = /:root:not\(\[data-theme="light"\]\)\s+\.toast\s*\{[^}]*background\s*:/;
      expect(blocks.some((b) => neutral.test(b))).toBe(true);
    });

    it(`${file} 的 [data-theme="dark"] 腿有中性 .toast 覆盖`, () => {
      expect(css).toMatch(/:root\[data-theme="dark"\]\s+\.toast\s*\{[^}]*background\s*:/);
    });

    // 中性覆盖不得把 .ok/.err 一起吃掉：.toast 选择器特异性 (0,3,0) 低于 .toast.ok 的 (0,4,0)，
    // 结构上安全；这里断言彩色态的暗色覆盖仍在，防止有人「顺手」删掉它们改用统一中性底。
    it(`${file} 中性覆盖没有取代 .ok/.err 的暗色覆盖`, () => {
      for (const variant of ['ok', 'err']) {
        expect(css).toMatch(
          new RegExp(`:root\\[data-theme="dark"\\]\\s+\\.toast\\.${variant}\\s*\\{`),
        );
      }
    });
  }
});

/**
 * mac vibrancy 让位链 —— 守 2026-07-21「窗口特效并未生效」的**失效模式**，而非它的具体样式取值。
 *
 * 该 bug 的本质不是「值写错了」，而是**规则悄悄不生效了**：port 层写的 mac 适配落在 `.win-frame` 上，
 * 而全仓根本不渲染这个 class ⇒ 选择器恒打空，CSS 看着有、实际是死的，真机查了很久才定位到。
 * 这一层（node env、无 CSSOM）验不了视觉，但「链条是否断」纯文本可断言，而且正是最容易再次悄悄断的地方。
 */
describe('mac 让位给原生 vibrancy：CSS 门控与 JS 写入必须成对存在', () => {
  const indexCss = stripComments(read('./index.css'));

  it('index.css 对 .stage 与 .win 都有 [data-window-effects="on"] 门控的透明规则', () => {
    // 两条缺任一条都会让 vibrancy 被挡：.stage 是最外层不透明 gradient，.win 是 86% 假毛玻璃。
    const letGo = [
      ...indexCss.matchAll(
        /:root\[data-os="mac"\]\[data-window-effects="on"\]\s+\.(stage|win)\s*\{[^}]*background\s*:\s*transparent/g,
      ),
    ].map((m) => m[1]);
    expect(letGo.sort()).toEqual(['stage', 'win']);
  });

  it('AppShell 真的会写 <html data-window-effects>（否则上面的门控恒不命中 = 又一处死规则）', () => {
    const shell = stripComments(read('../components/layout/AppShell.tsx'));
    expect(shell).toMatch(/setAttribute\(\s*['"]data-window-effects['"]/);
  });

  // 该属性描述「**这扇窗当初被建成什么样**」，不是「配置现在写着什么」：`transparent` 是
  // WebviewWindowBuilder 参数、运行期不可改，故必须一次性快照。改成跟随 config 会产生真 bug ——
  // 启动时特效关（窗口不透明 + 实色 #0B0F14）→ 设置里打开特效 → 属性翻 'on' → CSS 让位 →
  // 露出那块**深色**实底 → **浅色主题下深底配深字不可读**，要重启才恢复。
  it('data-window-effects 是一次性快照，不跟随 config 实时翻转', () => {
    const shell = stripComments(read('../components/layout/AppShell.tsx'));
    // 快照哨兵 + 早退：两者缺一，effect 就会在 config 变化时重写属性。
    expect(shell, '缺少快照 ref —— 属性会跟随 config 实时翻转').toMatch(
      /builtWindowEffects\s*=\s*useRef</,
    );
    expect(shell, '缺少「已快照即早退」的守卫').toMatch(
      /if\s*\(\s*builtWindowEffects\.current\s*!==\s*null\s*\)\s*return/,
    );
    // 且 setAttribute 必须在该早退之后 —— 顺序颠倒等于守卫不存在。
    const iGuard = shell.indexOf('builtWindowEffects.current !== null');
    const iSet = shell.indexOf("setAttribute('data-window-effects'");
    expect(iGuard, '快照守卫不见了').toBeGreaterThan(-1);
    expect(iSet, 'setAttribute 不见了').toBeGreaterThan(-1);
    expect(iGuard, '早退守卫必须在 setAttribute 之前，否则形同虚设').toBeLessThan(iSet);
  });

  /**
   * 🔴 上一条守卫**不够** —— 2026-07-21 独立复审实证：它只 grep「锁存语句存在」，验不出
   * 「已载入」判定本身是不是空的。当时写的是 `s.config !== undefined`，而 store 把 config
   * 声明为 `UserConfig | null` 且初始化为 `null`（app-store.ts:84 / :169）⇒ 判定恒为 true
   * ⇒ 首帧两个字段都是 undefined ⇒ 回落「默认开」⇒ 'on' 被锁存钉死。锁存本身没问题，
   * **喂给它的门是空的**，于是锁存把错值永久固化，比不加锁存更糟。
   *
   * 故这里断言的是**判定的正确性**，不是语句的存在性 —— 空门家族的通用教训：
   * 断言「结果对」之前，先断言「我真的检查到了东西」。
   */
  it('「config 已载入」判定必须对着 null 判（store 的空值是 null，不是 undefined）', () => {
    const shell = stripComments(read('../components/layout/AppShell.tsx'));
    const store = stripComments(read('../store/app-store.ts'));

    // 前提校验：store 的空值语义确实是 null。哪天改成 undefined，本条先红，逼人同步改上面的判定。
    expect(store, 'store 的 config 类型不再是 `UserConfig | null`？请同步本守卫').toMatch(
      /config:\s*UserConfig\s*\|\s*null/,
    );
    expect(store, 'store 的 config 初始值不再是 null？请同步本守卫').toMatch(/config:\s*null/);

    // 真正的断言：configLoaded 必须判 null；判 undefined 就是恒真的空门。
    //
    // 判据面从「读 `s.config` 的那个字面形态」放宽成「configLoaded 这条赋值的比较对象」：
    // 配置读点本轮起经 `useEffectiveConfig`（暂存回显层，见 `store/app-store.ts`），把读法写死在
    // 正则里会让本门跟着读层一起漂。**牙一点没减，反而更紧**：原来只管住 `useAppStore(...)` 那一种
    // 写法（换个读法即绕过负向断言、正向断言也只能陪着改），现在不论从哪层读，比较对象判成
    // `!== undefined` 一律红。空值语义仍由上面两条前提校验钉在 store 上。
    expect(shell, 'configLoaded 判成了 `!== undefined` = 恒真空门（store 的空值是 null）').not.toMatch(
      /configLoaded\s*=\s*[^;]*!==\s*undefined/,
    );
    expect(shell, 'configLoaded 必须对着 null 判').toMatch(/configLoaded\s*=\s*[^;]*!==\s*null/);
    expect(shell, 'configLoaded 必须真的来自配置读点，不能是别处凑的布尔').toMatch(
      /configLoaded\s*=\s*(?:useEffectiveConfig|useAppStore)\(/,
    );
  });

  /**
   * `'unknown'` 腿必须在**生产**里走得到 —— 此前 AppShell 是「未载入即 return」，
   * 于是 `resolveWindowEffectsState(undefined)` 只有单测能走到 = 生产死代码，
   * 而 index.css 的「属性缺失即不让位」兜底恰恰依赖它。单测覆盖了一条生产走不到的腿 = 假信心。
   */
  it('config 未载入时必须把 undefined 传给 resolve（让 unknown 腿在生产可达），且不锁存', () => {
    const shell = stripComments(read('../components/layout/AppShell.tsx'));
    expect(shell, '未载入时应传 undefined 而非提前 return').toMatch(
      /resolveWindowEffectsState\(\s*\n?\s*configLoaded\s*\?\s*\{[^}]*\}\s*:\s*undefined/,
    );
    expect(shell, "'unknown' 必须早退（不可锁存首帧的猜测）").toMatch(
      /if\s*\(\s*state\s*===\s*['"]unknown['"]\s*\)\s*return/,
    );
    // 早退必须在锁存赋值之前。
    const iUnknown = shell.indexOf("state === 'unknown'");
    const iLatch = shell.indexOf('builtWindowEffects.current = state');
    expect(iUnknown, "'unknown' 早退不见了").toBeGreaterThan(-1);
    expect(iLatch, '锁存赋值不见了').toBeGreaterThan(-1);
    expect(iUnknown, "'unknown' 早退必须在锁存之前").toBeLessThan(iLatch);
  });

  // 死选择器复发守卫：`.win-frame` 全仓无对应元素，任何样式表再出现它都是回到老坑。
  for (const file of ['./index.css', './components.css', './prototype.css', './screens.css']) {
    it(`${file} 不含已废弃的 .win-frame 选择器`, () => {
      expect(stripComments(read(file))).not.toMatch(/\.win-frame\b/);
    });
  }

  // 路 B 的前提：本仓靠「components.css 先 import、prototype.css 后 import」定层叠优先级，
  // 两份文件有 34 组同选择器规则取值冲突（含 .csel-menu 的 z-index 340↔1 这类功能性差异）。
  // 翻转 @import 顺序会一次性反转它们，且本层验不出来 —— 钉住顺序，逼任何翻转成为显式决策。
  it('index.css 的 @import 顺序：components.css 必须在 prototype.css 之前', () => {
    const iComponents = indexCss.indexOf("@import './components.css'");
    const iPrototype = indexCss.indexOf("@import './prototype.css'");
    expect(iComponents, "components.css 的 @import 不见了？").toBeGreaterThan(-1);
    expect(iPrototype, "prototype.css 的 @import 不见了？").toBeGreaterThan(-1);
    expect(iComponents).toBeLessThan(iPrototype);
  });
});

describe('rules.reverse 开/关文案对称', () => {
  const LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'] as const;

  it('结构自检：LOCALES 覆盖磁盘上全部语种文件（防漏语种 + 防 0 用例恒绿空转）', () => {
    // ⚠️ 原写法是 `expect(LOCALES.length).toBe(5)` —— 断言的是四行上刚声明的常量**自己**，
    // 恒真、零检出力（2026-07-21 独立复审判为自我断言）。真正要防的是「新增了语种却没进
    // LOCALES ⇒ 下面的循环悄悄漏测它」，所以断言对象必须是**磁盘实况**而非本地常量。
    const onDisk = readdirSync(fileURLToPath(new URL('../i18n/locales', import.meta.url)))
      .filter((f) => f.endsWith('.json'))
      .map((f) => f.replace(/\.json$/, ''))
      .sort();
    expect(onDisk.length, 'locales 目录扫不到文件 ⇒ 本条退化成恒绿').toBeGreaterThan(0);
    expect([...LOCALES].sort(), 'LOCALES 与磁盘上的语种文件不一致（新增语种要同步进来）').toEqual(
      onDisk,
    );
    for (const l of LOCALES) {
      const json = JSON.parse(read(`../i18n/locales/${l}.json`));
      expect(json.rules?.reverseOn, `${l} 缺 rules.reverseOn`).toBeTypeOf('string');
      expect(json.rules?.reverseOff, `${l} 缺 rules.reverseOff`).toBeTypeOf('string');
    }
  });

  // 「状态 · 语义」句式：开态早已是「回国/反向已开 · 本地走代理、海外直连」，关态曾是裸标题
  // 「回国/反向已关」—— 用户读不到关掉之后流量怎么走（真机反馈「文案差异也大」）。
  // 断言两条都带分隔符即可，不钉具体译文（译文可演进，句式不该退化）。
  for (const l of LOCALES) {
    it(`${l}: reverseOn / reverseOff 同为「状态 · 语义」两段式`, () => {
      const { rules } = JSON.parse(read(`../i18n/locales/${l}.json`));
      expect(rules.reverseOn, 'reverseOn 丢了语义子句').toContain(' · ');
      expect(rules.reverseOff, 'reverseOff 缺语义子句（开/关不对称即复发）').toContain(' · ');
    });
  }
});

describe('proxy-degraded 色阶：三处同语义必须同色（不得红/琥珀混用）', () => {
  /**
   * 被守的缺陷：「系统代理未生效」在同一屏上出现两种色阶 —— meta 行 `.home-degraded` 是 `--err` 红、
   * 降级横幅是 `.pending-bar.err` 红，而状态栏那颗点是 `--warn` 琥珀（2026-07-28 复审 LOW #7）。
   * 用户读作两个严重程度的事。
   *
   * 统一到 **warn**：降级不是错误 —— 核在跑，只是流量没经核（见 connection-state.ts）。
   * `--err` 红是「核崩了/起不来」那一档的词汇，借给降级会把两档压平。
   */
  const components = stripComments(read('./components.css'));
  const home = stripComments(read('../components/screens/home/HomeScreen.tsx'));
  const statusBar = stripComments(read('../components/layout/StatusBar.tsx'));

  it('meta 行 .home-degraded 用 --warn', () => {
    const rule = components.match(/\.home-degraded\s*\{([^}]*)\}/);
    expect(rule, '.home-degraded 规则不见了').not.toBeNull();
    expect(rule![1]).toMatch(/color\s*:\s*hsl\(var\(--warn\)\)/);
    expect(rule![1], '回到 --err 红 = 缺陷复发').not.toMatch(/--err/);
  });

  it('降级横幅不带 .err 修饰（`.pending-bar` 基线本就是 warn 调色）', () => {
    // 渲染点是 `connState === 'proxy-degraded'` 那个分支里的 <div className=...>。
    const banner = home.match(/connState === 'proxy-degraded' &&[\s\S]{0,200}?className="pending-bar[^"]*"/g);
    expect(banner, '降级横幅的渲染形态变了 —— 请同步本断言').not.toBeNull();
    for (const b of banner!) {
      expect(b, '降级横幅又挂回了 .err 红').not.toMatch(/className="pending-bar\s+err"/);
    }
  });

  it('状态栏那颗点仍是 warn（三处的锚点 —— 它一直是对的）', () => {
    expect(statusBar).toMatch(/connState === 'proxy-degraded'\s*\?\s*'warn'/);
  });
});

describe('pending-bar docked 全局位（P2）：搬家后必然踩的四个坑', () => {
  /**
   * 背景：待应用差集条从 `HomeScreen` 内联卡片迁到 `AppShell` `<main>` 底、贴 statusbar（spec §3.4 / U-6）。
   * 内联时代它被 `screens.css` 的 `#s-home > .pending-bar` 顶掉了全部 docked 专有属性；搬出 `#s-home`
   * 后那条 override 不再命中，`components.css` 里 `height:0` / `overflow:hidden` 这套**第一次真正生效**。
   *
   * 视觉结果只有真机能判，但**让它必然出错的结构**是纯文本可断言的，且正是最容易被后续批次改回去的那层。
   * 每条的变异对照写在用例里。
   */
  const shell = stripComments(read('../components/layout/AppShell.tsx'));
  const bar = stripComments(read('../components/layout/PendingChangesBar.tsx'));
  const componentsCss = stripComments(read('./components.css'));
  const screensCss = stripComments(read('./screens.css'));
  const homeTsx = stripComments(read('../components/screens/home/HomeScreen.tsx'));
  /** docked 补丁落在覆盖层（index.css:417 纪律：components.css / prototype.css 是禁区）。 */
  const overrideCss = stripComments(read('./index.css'));

  it('坑1 位置：条在 `.main-scroll` 之后、`<StatusBar />` 之前（U-6 排布，且不得回到滚动区内）', () => {
    const scroll = shell.indexOf('main-scroll');
    const pending = shell.indexOf('<PendingChangesBar />');
    const status = shell.indexOf('<StatusBar />');
    expect(pending, 'AppShell 没挂 PendingChangesBar').toBeGreaterThan(-1);
    // 变异对照：把 <PendingChangesBar /> 挪进 `.main-scroll` 的 children，或挪到 StatusBar 之后 → 转红。
    // 前者会让条随内容滚走（「常驻」名不副实），后者会把它压到状态栏下面。
    expect(pending, '条跑到 .main-scroll 之前/之内了').toBeGreaterThan(scroll);
    expect(pending, '条跑到状态栏下面了').toBeLessThan(status);
  });

  it('坑2 可见性：条必须自带 `.show` —— docked 基线是 height:0，不挂就是一条恒不可见的条', () => {
    // 变异对照：把 className 里的 'show' 去掉 → 转红。这是搬家最隐蔽的一坑：组件逻辑、事件、
    // IPC 全对，条就是不出现，而且 DOM 里查得到（height:0 + overflow:hidden）。
    expect(bar, 'PendingChangesBar 丢了 .show').toMatch(/'pending-bar show'/);
    // 前提校验：折叠基线确实还在（若哪天 .pending-bar 不再 height:0，本条的理由随之消失，请一并删）。
    expect(componentsCss).toMatch(/^\.pending-bar\{[^}]*height:0/m);
    expect(componentsCss).toMatch(/^\.pending-bar\.show\{[^}]*height:36px/m);
  });

  it('坑3 popover 不被裁：展开态放开 overflow + `.pd-pop` 自封顶', () => {
    // `.pending-bar{overflow:hidden}` 会把 absolute 子元素 `.pd-pop` 整块裁掉 = 点了没反应。
    // 变异对照：删掉 `.pending-bar.show{overflow:visible}` → 转红。
    expect(overrideCss, '.pd-pop 会被 .pending-bar 的 overflow:hidden 整块裁掉').toMatch(
      /\.pending-bar\.show\{[^}]*overflow:\s*visible/
    );
    // `.main` 是 overflow:hidden（叠加 container:mainc 的 layout containment ⇒ 裁切与层叠都收在 .main 内），
    // 向上弹的 popover 长过可用高度就顶穿上沿被裁且无法滚动。
    // 变异对照：删掉 max-height 或 overflow-y → 转红。
    const capped = overrideCss.match(/\.pd-pop\{[^}]*max-height[^}]*\}/);
    expect(capped, '.pd-pop 没有高度封顶 —— 差集一长就顶穿 .main 上沿').not.toBeNull();
    expect(capped![0], '封顶了却不给滚 = 超出部分永远看不到').toMatch(/overflow-y:\s*auto/);
  });

  it('坑4 向上弹：popover 用 `bottom` 锚定，不得再用 `top: calc(100% + …)`', () => {
    // 条 docked 在窗口底部贴状态栏 → 向下弹整块出屏（原型 :3308 同样是向上算的）。
    // 变异对照：把 bottom 改回 top → 转红。
    expect(bar, 'popover 没有向上锚定').toMatch(/bottom:\s*'calc\(100% \+ 6px\)'/);
    expect(bar, 'popover 又向下弹了（docked 位下会出屏）').not.toMatch(/top:\s*'calc\(100% \+/);
  });

  it('`.err` 不再是死态：由合成视图的 `err` 点亮，组件不另判一套', () => {
    // 原型 `.pending-bar.err`（prototype.css:554）此前零使用点。
    // 变异对照：把 className 里的 err 分支删掉 → 转红。
    expect(bar, '.err 又变回死 CSS').toMatch(/cn\('pending-bar show',\s*view\.err && 'err'\)/);
    // P4：红态判据搬进纯函数 `composeBarView`（§2.4 那张表的一列），组件只消费它。
    // 变异对照：在组件里另写一个 `applyFailed` 布尔来点红 → 与表分叉、转红。
    // 「红 ↔ toast 同源」那一半改由 `pending-bar-logic.test.ts` 的行为断言守（比正则更有牙）。
    expect(bar, 'apply 结果没走 applyOutcome（易与表分叉）').toMatch(/applyOutcome\(r\.status\)/);
  });

  it('`#s-home > .pending-bar` override 必须留着 —— 它现在是降级横幅的唯一支撑', () => {
    /**
     * 反直觉但关键：P2 的字面任务是「移除 `#s-home > .pending-bar` docked override」，
     * 但 `#s-home` 下的 `.pending-bar` **不止一处** —— 系统代理降级横幅（`connState === 'proxy-degraded'`）
     * 也借用了这个类。差集条搬走后该选择器已不再命中它，删掉这条规则不会「恢复原型形态」，
     * 只会把降级横幅塌成 height:0 + overflow:hidden 的隐形元素。
     * 变异对照：删掉该规则 → 转红。
     */
    expect(screensCss, '降级横幅的内联形态支撑没了').toMatch(
      /#s-home > \.pending-bar\{[^}]*height:\s*auto/
    );
    // 前提校验：`#s-home` 下确实还有一个 `.pending-bar` 消费者（若哪天降级横幅改用别的类，本规则应一并删）。
    expect(homeTsx, '#s-home 下已无 .pending-bar 消费者 —— 上面那条 CSS 该删了').toMatch(
      /className="pending-bar"/
    );
  });

  it('HomeScreen 不再渲染差集条（搬家不留双份）', () => {
    // 变异对照：在 HomeScreen 里把 <PendingChangesBar /> 加回去 → 转红（同一条会在首页出现两次）。
    expect(homeTsx).not.toMatch(/<PendingChangesBar\s*\/>/);
  });

  it('坑5 docked 补丁必须落在覆盖层，不得写进禁区文件 components.css', () => {
    /**
     * `components.css` 与 `prototype.css` 是原型逐字移植的禁区（index.css:417 明写「端口侧改动一律走
     * 本覆盖层」），改它们会让原型对拍失效。P2 的四条 docked 补丁最初写在了 components.css 里。
     * 变异对照：把任一条搬回 components.css → 转红。
     */
    expect(componentsCss, 'docked 补丁写进了禁区文件').not.toMatch(
      /\.pending-bar\.show\{[^}]*overflow:\s*visible/
    );
    expect(componentsCss, '.pd-pop 封顶写进了禁区文件').not.toMatch(
      /\.pd-pop\{[^}]*max-height/
    );
  });

  it('坑6 toast 让位：条展开时 bottom 必须再加一项条高，且与条的真实高度同源', () => {
    /**
     * 条 docked 后占据 statusbar 上方 36px，而 `#toast-stack` 的 bottom 只算了状态栏
     * ⇒ 结构上必然重叠（48px vs 条占 32–68px，压 20px）。内联时代不存在此问题，因为条被
     * `#s-home` override 成页内卡片、根本不在这个位置。
     *
     * 钉的是**关系**不是像素：条高必须由 `--pending-bar-h` 派生（写死 36 就是又造一笔死账 ——
     * 正是 `--statusbar-h` 那次修掉的同款坑），toast 的让位量必须引用同一个 var。
     */
    // ① var 存在且条的高度真的消费它（否则又是个没人用的孤儿常数）。
    expect(overrideCss, '--pending-bar-h 没定义').toMatch(/--pending-bar-h\s*:/);
    expect(overrideCss, '条高没走 --pending-bar-h —— 与 toast 的让位量会分叉').toMatch(
      /\.pending-bar\.show\{[^}]*height:\s*var\(\s*--pending-bar-h\s*\)/
    );
    // ② 让位规则存在，且只在条展开时生效（恒加 = 条不在时 toast 平白多浮 36px，就是原缺陷的镜像）。
    const yieldRule = overrideCss.match(/body:has\([^)]*\.pending-bar\.show[^)]*\)\s*#toast-stack\{([^}]*)\}/);
    expect(yieldRule, 'toast 没有给条让位 —— 二者必然重叠 20px').not.toBeNull();
    // ③ 让位量由两个 var 推导，且不含裸数值（写死即死账复发）。
    expect(yieldRule![1]).toMatch(/var\(\s*--statusbar-h\s*\)/);
    expect(yieldRule![1]).toMatch(/var\(\s*--pending-bar-h\s*\)/);
    expect(yieldRule![1], '让位量写死了像素').not.toMatch(/bottom\s*:\s*\d/);
  });
});

describe('csel 菜单的勾必须留在行内流（2026-07-30 真机：勾恒停在「域名正则」）', () => {
  /**
   * 缺陷不在 React：`.csel-opt.on` 与触发器文字取的是同一个 `value`，DOM 上 `on` 一直落在选中项。
   * 错的是**几何** —— prototype.css 给原生 `<select>` 外壳写的 `.sel svg{position:absolute;top:50%}`
   * 是**后代**选择器，把 `.sel.csel > .csel-menu > .csel-opt > .csel-ck` 里每一项的勾都绝对定位到
   * 包含块（`.csel-menu`，inline `position:fixed`）的垂直中线上；15 个勾同坐标重叠，只有选中项
   * `opacity:1` ⇒ 可见的勾恒画在「中线那一行」而非选中行（Playwright 实测 `top:149px`，300px 菜单
   * 未滚动时正落在第 4 项「域名正则」区间内，与用户描述逐字吻合）。
   *
   * 钉两件事，都是**关系**不是像素：
   *   ① 泄漏源仍在 prototype.css（禁区逐字镜像，改不得）⇒ 覆盖层的复位不能删；
   *   ② 复位规则特异性压得过 `.sel svg` 的 (0,1,1)，且落在 `@import` 之后。
   * 只钉 `position` 不够：静态定位后 `transform:translateY(-50%)` 仍会把勾上移半个身位，故 transform 同钉。
   */
  const protoCss = stripComments(read('./prototype.css'));
  const indexRaw = read('./index.css');
  const indexCss = stripComments(indexRaw);

  it('前提：prototype.css 仍有 `.sel svg` 后代绝对定位规则（泄漏源在 ⇒ 复位必须在）', () => {
    // 这条转红不代表出错，而是说明禁区文件已不再泄漏 —— 那时下面的复位才可以撤。
    expect(protoCss).toMatch(/\.sel\s+svg\s*\{[^}]*position\s*:\s*absolute/);
  });

  it('index.css 覆盖层把 .csel-ck 的 position 与 transform 复位，且特异性 ≥ 两个类', () => {
    const rules = [...indexCss.matchAll(/([^{}]+)\{([^}]*)\}/g)]
      .map((m) => ({ sel: m[1].trim(), body: m[2] }))
      .filter((r) => /\.csel-ck\b/.test(r.sel));
    expect(rules.length, '覆盖层里针对 .csel-ck 的规则一条都没有 —— 复位被删了').toBeGreaterThan(0);
    const reset = rules.find(
      (r) => /position\s*:\s*static/.test(r.body) && /transform\s*:\s*none/.test(r.body)
    );
    expect(reset, '.csel-ck 的 position/transform 复位缺失或不完整 —— 勾会再次脱离所在行').toBeDefined();
    // `.sel svg` = (0,1,1)；复位选择器至少两个类 ⇒ (0,2,0) 稳压，不依赖源序。
    expect(
      (reset!.sel.match(/\.[\w-]+/g) ?? []).length,
      '复位选择器只有一个类，特异性压不过 .sel svg'
    ).toBeGreaterThanOrEqual(2);
  });

  it('复位规则写在 index.css 的 @import 之后（写在前面会被 prototype.css 反压）', () => {
    // 位置比较必须在**去注释后**的文本上做：本层的根因注释里天然会引用 `@import` 这个词
    // （解释「为什么被后 @import 的 prototype.css 盖回」），在原文里找 lastIndexOf('@import')
    // 会命中注释、把基准点推到复位规则之后，让这条门凭空转红 —— 注释不该参与结构断言。
    const lastImport = indexCss.lastIndexOf('@import');
    const resetAt = indexCss.search(/^\s*[^{}\n]*\.csel-ck[^{}\n]*\{[^}]*position\s*:\s*static/m);
    expect(resetAt, '找不到复位规则').toBeGreaterThan(-1);
    expect(resetAt, '复位规则在 @import 之前 —— 同特异性时被后 import 的 prototype.css 覆盖').toBeGreaterThan(
      lastImport
    );
  });
});

describe('拓扑 tooltip 的位置由 JS 独占：prototype 的 transform 必须在覆盖层复位', () => {
  /**
   * 与上一组同一失效模式（「在 components.css 里对原型做的修正被后 @import 的 prototype.css 盖回」），
   * 但方向相反：上一组是 components **改写**了取值，这一组是 components **刻意不写** transform ——
   * 而「不写」在层叠里根本不构成取消，prototype.css:515 同选择器那份照样生效。
   *
   * 为什么必须复位：`clampToWrap`（lib/overlay-position.ts）返回的是 tooltip **左上角**坐标，已夹进
   * `[8, wrap.w - tipW - 8]`；组件只把它写进 inline left/top。CSS 再叠一个 `translate(-50%,-8px)`，
   * 视觉左缘就变成 `clamped_left - tipW/2` —— clamp 保下的 8px 内边距被吃穿，算得越准偏得越稳。
   * Playwright 实测：158px 宽的 tooltip 放在 clamp 下限上，溢出 `.sankey-wrap` 左侧 71px；
   * transform 置 none 后正好落在 wrap 左缘 +8px。
   *
   * 钉的是**关系**不是像素：泄漏源在 ⇒ 复位必须在；复位特异性压得过泄漏源；复位落在 @import 之后。
   */
  const protoCss = stripComments(read('./prototype.css'));
  const componentsCss = stripComments(read('./components.css'));
  const indexRaw = read('./index.css');
  const indexCss = stripComments(indexRaw);

  const skTipRule = (css: string) => {
    const m = [...css.matchAll(/(^|\})\s*\.sk-tip\s*\{([^}]*)\}/g)];
    return m.length ? m[0][2] : null;
  };

  it('前提：prototype.css 的 .sk-tip 仍带 transform，而 components.css 那份仍不带', () => {
    // 任一条不成立都说明前提变了（禁区文件不再泄漏 / 端口那份改了主意），届时下面的复位才可以重新评估。
    const proto = skTipRule(protoCss);
    expect(proto, 'prototype.css 里找不到 .sk-tip').not.toBeNull();
    expect(proto!, 'prototype.css 的 .sk-tip 已不带 transform —— 泄漏源没了').toMatch(/transform\s*:/);
    const comp = skTipRule(componentsCss);
    expect(comp, 'components.css 里找不到 .sk-tip').not.toBeNull();
    expect(comp!, 'components.css 的 .sk-tip 反而写了 transform —— 与端口 JS 定位相矛盾').not.toMatch(
      /transform\s*:/
    );
  });

  it('index.css 覆盖层把 .sk-tip 的 transform 复位，且特异性 ≥ 两个类', () => {
    const rules = [...indexCss.matchAll(/([^{}]+)\{([^}]*)\}/g)]
      .map((m) => ({ sel: m[1].trim(), body: m[2] }))
      .filter((r) => /\.sk-tip\b/.test(r.sel));
    expect(rules.length, '覆盖层里针对 .sk-tip 的规则一条都没有 —— 复位被删了').toBeGreaterThan(0);
    const reset = rules.find((r) => /transform\s*:\s*none/.test(r.body));
    expect(reset, '.sk-tip 的 transform 复位缺失 —— tooltip 会再次溢出卡片').toBeDefined();
    // 泄漏源 `.sk-tip` = (0,1,0)；复位至少两个类 ⇒ (0,2,0) 稳压，不依赖源序。
    expect(
      (reset!.sel.match(/\.[\w-]+/g) ?? []).length,
      '复位选择器只有一个类，特异性压不过 prototype 的 .sk-tip'
    ).toBeGreaterThanOrEqual(2);
  });

  it('复位规则写在 index.css 的 @import 之后（写在前面会被 prototype.css 反压）', () => {
    // 同上：在去注释文本上比位置，避免根因注释里的 `@import` 字样污染基准点。
    const lastImport = indexCss.lastIndexOf('@import');
    const resetAt = indexCss.search(/^\s*[^{}\n]*\.sk-tip[^{}\n]*\{[^}]*transform\s*:\s*none/m);
    expect(resetAt, '找不到复位规则').toBeGreaterThan(-1);
    expect(resetAt, '复位规则在 @import 之前 —— 同特异性时被后 import 的 prototype.css 覆盖').toBeGreaterThan(
      lastImport
    );
  });
});

describe('三处节点选择器的视觉统一：取值必须在同一条规则里，漏一处即红', () => {
  /**
   * 守的根因：「同一件事在三处各说一套」这类不一致，**纯逻辑单测永远抓不到** —— 三个组件各自
   * 全绿，视觉照样分叉。而它复发的具体形态是可预测的：有人为其中一处调选中色/组头字号，
   * 顺手只改那一处的选择器。
   *
   * 因此本门钉的是**结构**而非像素：
   *   ① 选中态的三份取值必须落在**同一条 CSS 规则**（选择器列表里同时出现三个行类）——
   *      拆开成三条、或只在其中一条上改值，本门立刻红；具体是 flow-weak 还是别的颜色不钉。
   *   ② 三个组件都必须真的**发出 `on` 这个类**（CSS 有规则但 JSX 不发 = 死规则，视觉照旧不一致）。
   *   ③ 三处节点行都必须真的画国旗（前置图标那一轴的落点）。
   *   ④ `.csel-ico` 的 position/transform 复位（prototype `.sel svg{position:absolute}` 泄漏的
   *      第四例，前三例见本文件 `.csel-ck` / `.sk-tip` / `.csel-grp-chev` 三段）。
   *   ⑤ 托盘组头的 uppercase 复位 + 特异性压得过 tray-overlay.css。
   *
   * 变异靶（逐条实跑过，见交付说明）：
   *   · 把统一规则的选择器列表里 `.tray-menu .tray-i.on` 那行删掉（= 只改主窗两处）→ ① 红；
   *   · 把 `TrayMenu.tsx` 里 `' on'` 拼接去掉（= CSS 改了组件没改）→ ② 红；
   *   · 把 `RuleDialog.tsx` 的 `FlagImg` 去掉（= 只给托盘补国旗）→ ③ 红；
   *   · 把 `.csel-ico` 复位删掉 → ④ 红；把托盘组头那条 `text-transform:none` 删掉 → ⑤ 红。
   */
  const indexRaw = read('./index.css');
  const indexCss = stripComments(indexRaw);
  const protoCss = stripComments(read('./prototype.css'));
  const componentsCss = stripComments(read('./components.css'));

  /** 拆成 `{选择器, 声明体}` —— 与本文件上方两段同一手法。 */
  const rules = (css: string) =>
    [...css.matchAll(/([^{}]+)\{([^}]*)\}/g)].map((m) => ({ sel: m[1].trim(), body: m[2] }));

  /** 三处的「节点行 / 选项行」类名 —— 统一态下它们必须共享同一条选中态规则。 */
  const ROW_ON = ['.nm-item.on', '.csel-opt.on', '.tray-i.on'] as const;

  /** 参与统一的三个组件（去注释后断言，本仓注释习惯逐字引用被改掉的旧形态）。 */
  const CONSUMERS = [
    '../components/screens/home/NodeMenu.tsx',
    '../tray/TrayMenu.tsx',
    '../components/dialogs/RuleDialog.tsx',
  ] as const;
  const consumers = CONSUMERS.map((rel) => ({ rel, src: stripComments(read(rel)) }));

  it('自检：CSS 与三个组件源码都读到了（防扫空恒绿）', () => {
    expect(indexCss.length).toBeGreaterThan(1000);
    expect(rules(indexCss).length).toBeGreaterThan(50);
    for (const { rel, src } of consumers) {
      expect(src.length, `${rel} 读空了 —— 被改名/移走了？`).toBeGreaterThan(2000);
      expect(src, `${rel} 去注释把源码吃光了`).toContain('import');
    }
  });

  it('① 选中态三处共享同一条规则（拆开或漏改任一处即红）', () => {
    const unified = rules(indexCss).filter((r) => r.sel.includes(ROW_ON[0]));
    expect(unified.length, '覆盖层里找不到 .nm-item.on 的规则 —— 统一段被删了').toBeGreaterThan(0);
    // 三个行类必须出现在**同一条**规则的选择器列表里：那是「不可能只改一处」的唯一结构保证。
    const shared = unified.find((r) => ROW_ON.every((cls) => r.sel.includes(cls)));
    expect(
      shared,
      `选中态取值没有落在同一条规则里（缺：${ROW_ON.filter(
        (c) => !unified.some((r) => r.sel.includes(c))
      ).join(' / ')}）—— 三处会各自演进`
    ).toBeDefined();
  });

  it('① 同一条规则里三件事都在：背景 + 文字色 + 字重（不钉具体色号）', () => {
    const shared = rules(indexCss).find((r) => ROW_ON.every((cls) => r.sel.includes(cls)))!;
    // 「选中的背景色」是本轮用户诉求的字面第一项：背景声明缺失 = 又退回「只换文字色」。
    expect(shared.body, '选中态缺 background —— 托盘那处的选中态会再次不可见').toMatch(/background\s*:/);
    expect(shared.body, '选中态缺 color').toMatch(/color\s*:/);
    expect(shared.body, '选中态缺 font-weight').toMatch(/font-weight\s*:/);
  });

  it('① hover/active 不得把选中态盖掉：复合选择器必须显式列出', () => {
    // `.csel-opt:hover`/`.nm-item.active` 与 `.csel-opt.on` 同为两个类档 ⇒ 只靠源序取胜太脆
    // （@import 顺序一变、或有人把本段挪到前面，选中行鼠标一扫就「掉色」成普通 hover）。
    const sel = rules(indexCss).find((r) => ROW_ON.every((c) => r.sel.includes(c)))!.sel;
    expect(sel, '缺 .on:hover 腿 —— 鼠标扫过选中行会掉色').toMatch(/\.on:hover/);
    expect(sel, '缺 .on.active 腿 —— csel 打开时键盘高亮会盖掉选中态').toMatch(/\.on\.active/);
  });

  it('② 三处的 `on` 类都真的发得出来（CSS 有规则、组件不发 = 死规则）', () => {
    // **发出点不等于消费方**：规则弹窗那处的 `.csel-opt.on` 是 `Csel` 内部按 value 命中发的，
    // RuleDialog 自己不发。把 RuleDialog 列进来会让本条被别处的 `on` 字样蒙过去（假绿），
    // 故这里换成三个真正的发出点。写法不钉（`&& 'on'` / `' on'` / `'on'` 等价），只钉「有字符串 on 类」。
    const ON_EMITTERS = [
      '../components/screens/home/NodeMenu.tsx', // .nm-item.on + .mi.on
      '../tray/TrayMenu.tsx', // .tray-i.on
      '../components/dialogs/Csel.tsx', // .csel-opt.on
    ] as const;
    for (const rel of ON_EMITTERS) {
      const src = stripComments(read(rel));
      expect(src.length, `${rel} 读空了`).toBeGreaterThan(2000);
      expect(src, `${rel} 没有 'on' 类的发出点 —— 选中态在这一处不生效`).toMatch(/['"]\s?on['"]/);
    }
  });

  it('③ 三处节点行都画国旗（前置图标那一轴的落点，同一渲染器 + 同一数据源）', () => {
    for (const { rel, src } of consumers) {
      expect(src, `${rel} 不再画国旗 —— 前置图标这一轴又分叉了`).toContain('FlagImg');
      expect(src, `${rel} 国旗数据源不是名称派生（三处必须同源）`).toContain('flagCodeForName');
    }
  });

  it('④ 前提：prototype.css 的 `.sel svg` 仍泄漏 ⇒ `.csel-ico` 复位不能删', () => {
    // 这条转红不代表出错，而是说明禁区文件已不再泄漏，届时复位才可以撤（同本文件 `.csel-ck` 段）。
    expect(protoCss).toMatch(/\.sel\s+svg\s*\{[^}]*position\s*:\s*absolute/);
    const reset = rules(indexCss).find(
      (r) => /\.csel-ico\b/.test(r.sel) && /position\s*:\s*static/.test(r.body)
    );
    expect(reset, '.csel-ico 的 position 复位缺失 —— 新加的前置图标会被绝对定位到菜单中线上重叠').toBeDefined();
    expect(reset!.body, '.csel-ico 缺 transform 复位 —— 图标仍会上移半个身位').toMatch(/transform\s*:\s*none/);
    expect(
      (reset!.sel.match(/\.[\w-]+/g) ?? []).length,
      '复位选择器只有一个类，特异性压不过 .sel svg 的 (0,1,1)'
    ).toBeGreaterThanOrEqual(2);
  });

  it('④ 有图标后 label 的截断改钉在 `.csel-lbl` 类上（`:first-child` 已命不中）', () => {
    // prototype.css:1540 的 `.csel-opt > span:first-child` 给的 flex:1 + 省略号，在图标插到前面后
    // 命中的是图标而不是 label ⇒ 长选项会撑爆菜单而不是截断。
    expect(protoCss, '前提变了：prototype 不再用 :first-child 定 label').toMatch(
      /\.csel-opt\s*>\s*span:first-child/
    );
    const lbl = rules(indexCss).find((r) => /\.csel-lbl\b/.test(r.sel));
    expect(lbl, '.csel-lbl 的 flex/截断规则缺失 —— 长选项会撑爆菜单').toBeDefined();
    expect(lbl!.body).toMatch(/text-overflow\s*:\s*ellipsis/);
  });

  it('⑤ 托盘组头对齐基准：uppercase 必须复位，且特异性压过 tray-overlay.css', () => {
    // 组名是**用户数据**（订阅名），大写化等于改写用户自己起的名字（规则弹窗那处同因已复位）。
    const grp = rules(indexCss).filter((r) => /\.tray-grp-t\b/.test(r.sel));
    expect(grp.length, '覆盖层里没有托盘组头的对齐规则').toBeGreaterThan(0);
    const cased = grp.find((r) => /text-transform\s*:\s*none/.test(r.body));
    expect(cased, '托盘组头的 uppercase 没复位 —— 订阅名会被大写化').toBeDefined();
    // tray-overlay.css 在 index.css **之后** @import（见 tray/main.tsx），故必须靠特异性取胜：
    // 它那条是 `.tray-menu .tray-grp-t` = 两个类档，本条至少要三个。
    expect(
      (cased!.sel.match(/\.[\w-]+/g) ?? []).length,
      '托盘组头对齐规则的类数 < 3，压不过后 @import 的 tray-overlay.css'
    ).toBeGreaterThanOrEqual(3);
  });

  it('⑤ 前提：禁区两文件都没给这三个行类写过选中背景（写了就该重新评估覆盖层）', () => {
    for (const [name, css] of [['prototype.css', protoCss], ['components.css', componentsCss]] as const) {
      for (const cls of ROW_ON) {
        const owned = rules(css).filter((r) => r.sel.includes(cls));
        for (const r of owned) {
          expect(
            r.body,
            `${name} 的 ${cls} 现在自带 background —— 覆盖层那条统一规则需要重新评估`
          ).not.toMatch(/background\s*:/);
        }
      }
    }
  });

  it('统一段整体落在最后一个 @import 之后（写在前面会被 prototype.css 反压）', () => {
    // 位置比较必须在**去注释后**的文本上做：本段的根因注释里天然会引用 `@import` 这个词。
    const lastImport = indexCss.lastIndexOf('@import');
    const at = indexCss.search(/^[^{}\n]*\.nm-item\.on[^{}]*\{/m);
    expect(at, '找不到统一规则').toBeGreaterThan(-1);
    expect(at, '统一段在 @import 之前 —— 同特异性时被后 import 的 prototype.css 覆盖').toBeGreaterThan(
      lastImport
    );
  });
});

describe('「阻断」配色两轴：动作标签轴恒 --err 且常驻，流量表达轴恒 --warn', () => {
  /**
   * 被守的缺陷：同一件事（阻断）在同一个应用里漂成**四档** —— 常驻 --err / 只有 hover 才 --err /
   * 完全无色 / --warn。四档不是有人调错了色，是**没有任何门约束过它**（本文件此前 9 个 describe
   * 一条都没提 block），所以每加一处就多一种说法。
   *
   * 陈先生 2026-07-30 裁定的两轴（设计稿 polaris-rule-dialog-redesign.md §5.4）：
   *  · **动作标签轴** =「这条规则/这个应用/这个出口的动作是阻断」「选中它会阻断」→ 恒 `--err`，且**常驻**
   *    （idle 就得显示危险度：菜单项要 hover 才知道点下去会断网，那是缺陷不是风格）。
   *  · **流量表达轴** =「流量到此被丢弃」（拓扑那条 block 出口条）→ `--warn`。它随任何常驻 block 规则
   *    永久存在，用 --err 会把「需要你注意」脱敏（同 proxy-degraded 那道门的推理，见本文件 :247）。
   *
   * ⚠️ **本门的射程与恒绿边界（如实记账）**：
   *  · ①③④⑤⑥⑦ 钉的是**已注册的 13 处**的取值与形态 —— 任一处改用第二个 token、或把常驻改回
   *    hover-only、或把 danger 通道拆掉，立刻红。这几条是真有牙的。
   *  · ⑧ 是**新增点的捕网**，靠的是 i18n：「阻断」这两个字禁硬编码（仓规），任何第 N 处渲染它都必须
   *    引用那几个 key ⇒ 引用者文件集合就是花名册，多一个文件即红，逼作者来这里登记并声明取哪一档。
   *    它**抓不到**两种情形，别把它当全覆盖：(a) 不显示「阻断」二字、只画一个红/琥珀图形的新点；
   *    (b) 托盘那套自带的 `t(zh, en)`（不走 i18next），故托盘单列一条 ⑤ 专断言。
   *
   * 变异靶（12 个，逐条实跑过，**每条都真的把对应腿打红**，无恒绿腿）：
   *  · index.css 常驻规则 `--err`→`--warn` ⇒ ① 红｜整条删掉（退回 hover-only）⇒ ② 红；
   *  · 常驻红改钩通用 `.tray-i.danger` ⇒ ②-越界 红（那会把托盘「退出」一并涂成常驻红）；
   *  · `RuleDialog` 阻断项删 `danger: true` ⇒ ③ 红｜`Csel` 不落 `.csel-opt.danger` ⇒ ③ 红｜
   *    `Csel` 触发器不吃 danger ⇒ ③ 红；
   *  · `StatusBar` 或 `NodeMenu` 删 `act-block-txt` ⇒ ④ 红｜托盘把它挪到「退出」行 ⇒ ④+⑤ 红；
   *  · 连接表 blocked chain `--err`→`--warn` ⇒ ⑥ 红；
   *  · `topology-layout.ts` 的 `COLOR_BLOCKED` 改回 `--err` ⇒ ⑦ 红；
   *  · 新建一个组件渲染 `t('rules.targetBlock')` ⇒ ⑧ 红。
   */
  const STYLE_FILES = ['./screens.css', './index.css', './components.css', './prototype.css'] as const;

  /** 「这条 CSS 规则在给『阻断』这件事上色」的选择器标记（动作标签轴）。 */
  const BLOCK_SELECTOR_MARKERS = [
    '.act-block-txt', // 常驻红的载体：三处菜单行 + 状态栏/首页两处出口名文本
    '.act-block', // pill：规则行 / 应用分流 / 两张 hover 卡
    '.act-dot.block', // 应用分流策略色点
    '.mi.danger', // 三处菜单行的 hover 增量腿（同轴，色必须同源）
    '.tray-i.danger', // 同上（托盘）
    '.csel-opt.danger', // 规则弹窗「目标出站」选项（Csel 通用危险度通道，唯一消费方）
    '.csel-trigger.danger', // 同上，菜单关着时的选中态载体
  ] as const;

  /** 常驻红的三个菜单落点 —— 必须钩 `.act-block-txt` 而非 `.danger`（`.danger` 托盘退出也戴）。 */
  const RESIDENT_MENU_SELECTORS = [
    '.node-menu .mi.act-block-txt',
    '.mini-menu .mi.act-block-txt',
    '.tray-menu .tray-i.act-block-txt',
  ] as const;

  /** 拆成 `{选择器, 声明体}`（与本文件上方两段同一手法）。 */
  const rules = (css: string) =>
    [...css.matchAll(/([^{}]+)\{([^}]*)\}/g)].map((m) => ({ sel: m[1].trim(), body: m[2] }));

  /**
   * 一条规则里**属于动作标签轴**的选择器分支。
   *
   * 带 `.on` 的分支要剔除：`.node-menu .mi.danger.on:hover` 落在「三处节点选择器选中态统一」那条
   * 全仓共享规则里（本文件 :522 那道门在守它），取 flow-weak/flow-hi 是**刻意的** —— 选中态的红
   * 改由 `.csel-trigger.danger` 承载（见 Csel.tsx `currentDanger` 头注）。不剔除的话本门会把那道
   * 门的正确行为判成漂移。
   */
  const axisBranches = (sel: string) =>
    sel
      .split(',')
      .map((s) => s.trim())
      .filter((s) => BLOCK_SELECTOR_MARKERS.some((m) => s.includes(m)) && !s.includes('.on'));

  /** 声明体里被引用的颜色 token（只取 `hsl(var(--x))` 形态，跳过 --r-sm / --sp-2 这类几何令牌）。 */
  const colorTokens = (body: string) =>
    [...body.matchAll(/hsl\(\s*var\(\s*(--[\w-]+)/g)].map((m) => m[1]);

  /** 动作标签轴的全部 CSS 着色点（跨四个样式文件）。 */
  const axisRules = STYLE_FILES.flatMap((f) => {
    const css = stripComments(read(f));
    return rules(css)
      .filter((r) => axisBranches(r.sel).length > 0)
      .map((r) => ({ file: f, sel: r.sel, tokens: colorTokens(r.body) }))
      .filter((r) => r.tokens.length > 0);
  });

  const readSrc = (rel: string) => stripComments(read(rel));

  it('自检：四个样式文件都读到了，且动作标签轴真的扫出了着色点（防扫空恒绿）', () => {
    for (const f of STYLE_FILES) expect(read(f).length, `${f} 读空了`).toBeGreaterThan(1000);
    expect(axisRules.length, '一条 block 着色规则都没扫到 —— 标记表过期了？').toBeGreaterThanOrEqual(8);
  });

  it('① 动作标签轴的全部 CSS 着色点只用 err 族 token（第二个 token = 第五档开张）', () => {
    const offenders = axisRules
      .flatMap((r) => r.tokens.map((tok) => ({ ...r, tok })))
      .filter((r) => !/^--err(-weak)?$/.test(r.tok));
    expect(
      offenders.map((o) => `${o.file} 「${o.sel}」用了 ${o.tok}`),
      '动作标签轴混进了非 err token —— 阻断又漂成两种严重程度'
    ).toEqual([]);
  });

  it('② 三处菜单项的危险色必须**常驻**（只有 :hover 腿 = 要 hover 才知道会断网）', () => {
    const indexCss = stripComments(read('./index.css'));
    for (const marker of RESIDENT_MENU_SELECTORS) {
      const resident = rules(indexCss).some(
        (r) =>
          r.sel.split(',').some((s) => s.trim() === marker) &&
          /color\s*:\s*hsl\(var\(--err\)\)/.test(r.body)
      );
      expect(resident, `${marker} 没有常驻的 --err 腿 —— 退回 hover-only 了`).toBe(true);
    }
  });

  it('② 常驻红不得钩到通用 `.danger` 上（会把托盘「退出」一并涂红，越出裁定射程）', () => {
    const indexCss = stripComments(read('./index.css'));
    const overreach = rules(indexCss).filter(
      (r) =>
        r.sel
          .split(',')
          .some((s) => /(^|\s)\.(tray-i|mi)\.danger$/.test(s.trim()) || s.trim() === '.danger') &&
        /color\s*:\s*hsl\(var\(--err\)\)/.test(r.body)
    );
    expect(
      overreach.map((r) => r.sel),
      '常驻红钩到了无 :hover 的通用 .danger —— 托盘退出等破坏性项会跟着变常驻红'
    ).toEqual([]);
  });

  it('③ 规则弹窗那一处走 Csel 的 danger 通道（通道→渲染→调用点，缺一即死规则）', () => {
    // 通道：字段在结构类型上（不在这里，`row.opt.danger` 就取不到）。
    expect(readSrc('../components/dialogs/csel-logic.ts'), 'CselOptionLike 丢了 danger 通道').toMatch(
      /danger\?\s*:\s*boolean/
    );
    const csel = readSrc('../components/dialogs/Csel.tsx');
    expect(csel, 'Csel 不再把 danger 落成 .csel-opt.danger —— CSS 成了死规则').toMatch(
      /csel-opt[^`]*danger/
    );
    expect(csel, 'Csel 触发器不再吃 danger —— 菜单关着时「已选阻断」又变回无色').toMatch(
      /csel-trigger\$\{[^`]*danger/
    );
    // 调用点：block 那一项必须真的声明危险度。
    const dlg = readSrc('../components/dialogs/RuleDialog.tsx');
    const blockOpt = dlg.match(/value:\s*'block'[\s\S]{0,300}?\}/);
    expect(blockOpt, 'RuleDialog 的 block 选项形态变了 —— 请同步本断言').not.toBeNull();
    expect(blockOpt![0], 'block 选项没声明 danger —— 规则页又是全仓唯一不红的那处').toMatch(
      /danger:\s*true/
    );
  });

  it('④ 五处必须真的发出 .act-block-txt（CSS 在、组件不发 = 死规则，视觉照旧无色）', () => {
    for (const rel of [
      '../components/layout/StatusBar.tsx', // 状态栏出口名
      '../components/screens/home/HomeScreen.tsx', // 首页 #cur-node 出口名
      '../components/screens/home/NodeMenu.tsx', // 首页出口选单阻断项
      '../components/screens/apppolicy/AppPolicyScreen.tsx', // 应用分流策略菜单阻断项
      '../tray/TrayMenu.tsx', // 托盘出口选单阻断项
    ]) {
      expect(readSrc(rel), `${rel} 不再发出 .act-block-txt`).toContain('act-block-txt');
    }
  });

  it('⑤ 托盘那处必须落在**阻断**行上（同文件的「退出」也是 tray-i danger，裸串查会恒绿）', () => {
    const tray = readSrc('../tray/TrayMenu.tsx');
    expect(
      tray,
      '托盘的 act-block-txt 不在阻断行上 —— 可能挂到了「退出」那颗 tray-i danger'
    ).toMatch(/tray-i danger act-block-txt\$\{blockSelected/);
  });

  it('⑥ 两处内联着色点仍是 --err（连接表 chain 列 / 应用分流汇总行）', () => {
    const conn = readSrc('../components/screens/connections/ConnectionsScreen.tsx');
    const branch = conn.match(/blocked \?[\s\S]{0,200}?<\/span>/);
    expect(branch, '连接表 blocked 分支形态变了 —— 请同步本断言').not.toBeNull();
    expect(branch![0], '连接表 blocked chain 不再是 --err').toContain('hsl(var(--err))');
    expect(branch![0], '连接表 blocked chain 改用 --warn = 跨轴').not.toContain('--warn');

    const ap = readSrc('../components/screens/apppolicy/AppPolicyScreen.tsx');
    const sum = ap.match(/\{[^{}]*color:[^{}]*\}\}[\s\S]{0,120}?appPolicy\.summary\.block/);
    expect(sum, '应用分流汇总行「阻断 N」形态变了 —— 请同步本断言').not.toBeNull();
    expect(sum![0], '汇总行「阻断 N」不再是 --err').toContain('hsl(var(--err))');
  });

  it('⑦ 流量表达轴（拓扑 block 出口条）恒 --warn，不得跨到 --err', () => {
    const topo = stripComments(read('../components/screens/home/topology-layout.ts'));
    const decl = topo.match(/COLOR_BLOCKED\s*=\s*'([^']+)'/);
    expect(decl, 'COLOR_BLOCKED 不见了 —— 拓扑阻断色改由别处决定？').not.toBeNull();
    expect(decl![1], '拓扑 block 条跨到了动作标签轴的红').toBe('hsl(var(--warn))');
  });

  it('⑧ 「阻断」标签的引用者文件集合 == 花名册（新增第 N 处 → 红，逼你来这里登记取哪一档）', () => {
    /** 用户可见的「阻断」文案 key（禁硬编码 ⇒ 任何渲染点都必须引用其中之一）。 */
    const BLOCK_LABEL_KEYS = [
      'rules.targetBlock',
      'appPolicy.action.block',
      'appPolicy.summary.block',
      'home.routingBlock',
    ];
    /** 已登记的引用者 + 各自在轴上的落点（改动本表时请一并核对上面 ①–⑦）。 */
    const ROSTER = [
      'components/dialogs/RuleDialog.tsx', //        动作标签轴 · Csel danger 通道
      'components/hover-cards/AppRuleHoverCard.tsx', // 动作标签轴 · .act-block pill
      'components/hover-cards/RuleHoverCard.tsx', //  动作标签轴 · .act-block pill
      'components/layout/StatusBar.tsx', //           动作标签轴 · .act-block-txt
      'components/screens/apppolicy/AppPolicyScreen.tsx', // pill + 色点 + .mi.danger + 汇总行
      'components/screens/connections/ConnectionsScreen.tsx', // 动作标签轴 · blocked chain 内联 --err（见 ⑥）
      'components/screens/home/HomeScreen.tsx', //    动作标签轴 · .act-block-txt（#cur-node）
      'components/screens/home/NodeMenu.tsx', //      动作标签轴 · .mi.danger
      'components/screens/rules/RuleItem.tsx', //     动作标签轴 · .act-block pill
    ];

    const root = fileURLToPath(new URL('..', import.meta.url));
    const walk = (dir: string): string[] =>
      readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
        e.isDirectory() ? walk(`${dir}/${e.name}`) : [`${dir}/${e.name}`]
      );
    const found = walk(root)
      .filter((p) => /\.tsx?$/.test(p) && !/\.test\.tsx?$/.test(p))
      .filter((p) => {
        const src = readFileSync(p, 'utf8');
        return BLOCK_LABEL_KEYS.some((k) => src.includes(k));
      })
      .map((p) => p.slice(root.length).replace(/^\//, ''))
      .sort();

    expect(found.length, '一个引用者都没扫到 —— 走查逻辑失效了（恒绿）').toBeGreaterThan(4);
    expect(
      found,
      '「阻断」多了/少了渲染点。多：在 ROSTER 登记它，并按两轴给它定色（动作标签→--err 常驻）；少：删掉对应行。'
    ).toEqual(ROSTER);
  });
});

describe('chip 族选中态的浅色分离度：它靠的是「兄弟 chip 是 surface」，那条底色不能被消掉', () => {
  /**
   * 2026-07-30 实测（dist 产物 CSS + Chromium computedStyle + WCAG/CIE L*，数据逐条写在
   * styles/index.css「C) chip 族的浅色实测」段）：`.tagchip.on` 在浅色下**没有** A 族那个
   * 「flow-weak 选中块糊进 surface-2 轨道」的问题 —— 它的参照系是同排未选 chip 的 surface(L100%)，
   * ΔL*=5.29 / 1.14:1，与 A 族**已接受**的修法（5.51 / 1.15:1）同档，是**被否决**那一档
   * （0.22 / 1.01:1）的 24 倍。结论是「不改」。
   *
   * 但这个结论**有一条载荷**，必须钉住：`.tagchip` 的 idle 底是 `--surface` 而不是 `--surface-2`，
   * 靠的是 `prototype.css`（index.css 的**最后一个 @import**）压过 `components.css` 里同选择器的
   * `background:hsl(var(--surface-2))`。这份重复看着像「该消掉的冗余」，谁真去消掉 prototype 那行，
   * tagchip 的选中态就从 ΔL*=5.29 掉到 0.22 —— 精确落回被否决的那一档，而且**没有任何别的信号**
   * 会提示这件事（视觉上就是「选中的标签看不出选中了」，得真机盯着才发现）。
   *
   * 变异靶（实跑过）：
   *  · 把 prototype.css `.tagchip` 的 `--surface` 改成 `--surface-2` → ② 红；
   *  · 把 index.css 的 @import 顺序改成 prototype 不在最后 → ① 红。
   */
  const indexCss = stripComments(read('./index.css'));

  it('① prototype.css 仍是 index.css 的最后一个 @import（顺序一变，下面那条压制就反转）', () => {
    const imports = [...indexCss.matchAll(/@import\s+'([^']+)'/g)].map((m) => m[1]);
    expect(imports.length, '@import 一个都没扫到').toBeGreaterThan(2);
    expect(imports[imports.length - 1], 'prototype.css 不再是最后一个 @import').toBe('./prototype.css');
  });

  it('② `.tagchip` 的 idle 底必须解析到 --surface（= 同排兄弟是白块，选中的 flow-weak 才分得出来）', () => {
    const proto = stripComments(read('./prototype.css'));
    const rule = proto.match(/(?:^|\})\s*\.tagchip\s*\{([^}]*)\}/);
    expect(rule, 'prototype.css 的 .tagchip 规则不见了 —— 那份「重复」被消掉了？').not.toBeNull();
    expect(
      rule![1],
      '.tagchip idle 底不再是 --surface：选中块 flow-weak 会与兄弟等亮（ΔL* 5.29 → 0.22），' +
        '正是 .geo-region 那次被否决的形态。要改先重测，别只看「消掉了一份重复」。'
    ).toMatch(/background\s*:\s*hsl\(var\(--surface\)\)/);
  });

  it('③ 前提自检：chip 族的选中态仍是 flow-weak 底（换了底色，上面两条的实测结论要重做）', () => {
    const proto = stripComments(read('./prototype.css'));
    for (const cls of ['.tagchip.on', '.ap-chip.on', '.aad-proc-opt.on']) {
      const rule = proto.match(new RegExp(`${cls.replace(/\./g, '\\.')}\\s*\\{([^}]*)\\}`));
      expect(rule, `${cls} 规则不见了`).not.toBeNull();
      expect(rule![1], `${cls} 的选中底换了 —— index.css 那段浅色实测结论需要重测`).toMatch(
        /background\s*:\s*hsl\(var\(--flow-weak\)\)/
      );
    }
  });
});

describe('.nd-card transition 收窄覆盖层：靠位于全部 @import 之后取胜，同型的第 7 处', () => {
  /**
   * 背景：`.nd-card` 的 `transition` 在 screens.css:61（工作文件）已收窄到六个不参与盒模型的
   * 绘制层属性，但 `prototype.css:682` 有逐字同选择器、同特异性 (0,1,0) 的未收窄声明
   * （`transition:.14s` = 无 property 限定 ⇒ `transition-property:all`），而 prototype.css 是
   * index.css 的最后一个 @import ⇒ 同特异性下后者胜，会把 screens.css 的收窄压回 `all`。
   * 真正让收窄生效的是位于全部 @import 之后、index.css 里的覆盖层。
   *
   * 本仓同型结构（「靠位于全部 @import 之后取胜的覆盖层」）在本文件已有实测过的先例——
   * :497/:560/:726 三处断言的是同一件事（复位/统一规则必须落在最后一个 @import 之后）；
   * :253/:986 是相邻但不同的一类（钉的是 @import **顺序**本身：components 在 prototype 之前、
   * prototype 是最后一个），本组的③直接复用后一类的判据。这是第 4 处「覆盖落在 @import 之后」
   * 型的门——补齐后才有护栏防止：覆盖层被挪位、被后续段落覆盖、或 prototype.css 再同步时新增
   * 一条更靠后的 `.nd-card` transition 声明，任一种都会静默把收窄退回 `all`，而 NodesScreen.tsx
   * 四处留档仍写着「已收窄」，gate 全绿没人发现。
   *
   * 2026-08-17 二轮复审追加：①②③④⑤ 原先对「取值内容」零覆盖——②只断言两处**相等**，没有任何
   * 一条断言相等的取值本身是「收窄过的」。实跑变异：把 screens.css 与 index.css 的 transition
   * **同步**改回 `transition:.14s;`，①②③④⑤ 整组仍绿（位置对、两处也相等，只是又等成了 `all`）。
   * ⑥补上内容判据堵住这条。
   *
   * 同轮还发现 ④⑤ 用 `find` 取第一条匹配、且 `filter` 条件里混了 body 内容（`/border-color/`），
   * 覆盖层之后再插一条同选择器规则（内容上不含 "border-color" 这个子串，例如整条重新
   * `border:0`）不会被「恰好一条」发现，而层叠里更晚出现的那条会赢，缺陷原样复发但门不知道。
   * 已改成纯按选择器 `filter` + 断言恰好一条（与①的 transition 覆盖同款）。
   *
   * F1（2026-08-17 复审，同批修）：列表档 `border:0`/`border-bottom:0` 是简写，未提到的
   * border-*-color 会复位成 `currentcolor`（近黑/近白），叠加收窄后 border-color 仍按 140ms
   * 渐变、border-width 瞬切 ⇒ **离开**列表档（list→card，侧边框 0→1px 瞬现）以及**失去
   * `:last-child` 身份**（底边框 0→1px 瞬现）时会先闪一道近黑/近白边框再淡到 --line/--hair
   * （方向如实记：不是「进入」列表档那一侧——那一侧是宽度瞬间归零，currentcolor 根本没机会上屏）。
   * 修法是 index.css 里两条显式钉死 border-color 的覆盖，同样靠 @import 之后取胜，同样需要门
   * 守住——否则挪位、被覆盖、或规则体内补一行 `border:0` 简写（层叠里更晚的声明赢，即使写在
   * 同一条规则内也一样）都会让缺陷原样复发且没有任何信号。
   *
   * 变异靶（实跑过，见交接说明）：
   *  · 把 index.css 的 `.nd-card{transition:…}` 覆盖挪到 @import 之前 → ① 红；
   *  · 改 screens.css 或 index.css 任一处的 transition 值但不同步另一处 → ② 红；
   *  · 两处**同步**改回 `transition:.14s;`（退回 `all` 但位置/相等两条前提仍满足）→ ⑥ 红；
   *  · 把 index.css 里 F1 补的两条 border-color 覆盖挪到 @import 之前 → ④ 红；
   *  · 把这两条覆盖的取值改回含 `currentcolor`（或删掉其中一条） → ⑤ 红；
   *  · 在列表档基础态覆盖规则**内**追加一行 `border:0;`（同规则内简写复位颜色）→ ⑤ 红；
   *  · 在覆盖层之后再插一条同选择器的 `#s-nodes.nodes-list-view .nd-card{ border:0; }` → ④/⑤ 红
   *    （"恰好一条"断言先炸，即使凑巧没炸，body 里出现的 `border:` 简写也会被 ⑤ 的新增断言拦下）。
   */
  const indexCss = stripComments(read('./index.css'));
  const screensCss = stripComments(read('./screens.css'));
  const lastImport = indexCss.lastIndexOf('@import');

  /** index.css 里带 `transition` 的 `.nd-card` 覆盖——预期恰好一条。 */
  const ndCardTransitionOverride = () => {
    const hits = flat(indexCss).filter(
      (r) => r.sel === '.nd-card' && /transition\s*:/.test(r.body),
    );
    expect(hits.length, 'index.css 里带 transition 的 .nd-card 覆盖不是恰好一条').toBe(1);
    return hits[0];
  };
  const extractTransition = (body: string) =>
    body.match(/transition\s*:\s*([^;]+);/)?.[1].replace(/\s+/g, ' ').trim();

  it('① .nd-card 的 transition 收窄覆盖落在 index.css 全部 @import 之后', () => {
    expect(lastImport, 'index.css 里一个 @import 都没有 —— 走查逻辑失效了').toBeGreaterThan(0);
    const rule = ndCardTransitionOverride();
    expect(
      indexCss.indexOf(rule.body),
      '覆盖写在 @import 前面 —— 会被 prototype.css 同特异性反压，等于空操作',
    ).toBeGreaterThan(lastImport);
  });

  it('② screens.css 与 index.css 两处 transition 取值逐字相等（手工承诺 → 机器承诺）', () => {
    const screensRule = flat(screensCss).find(
      (r) => r.sel === '.nd-card' && /transition\s*:/.test(r.body),
    );
    expect(screensRule, 'screens.css 的 .nd-card 规则不见了').toBeTruthy();
    const screensValue = extractTransition(screensRule!.body);
    const overrideValue = extractTransition(ndCardTransitionOverride().body);
    expect(screensValue, 'screens.css 侧没解析出 transition 取值').toBeTruthy();
    expect(overrideValue, '覆盖层没解析出 transition 取值').toBe(screensValue);
  });

  it('③ 前提自检：prototype.css 仍是最后一个 @import（同 :986，钉在本组内避免跨 describe 失联）', () => {
    const imports = [...indexCss.matchAll(/@import\s+'([^']+)'/g)].map((m) => m[1]);
    expect(
      imports[imports.length - 1],
      'prototype.css 不再是最后一个 @import ⇒ 上面两条的前提不成立',
    ).toBe('./prototype.css');
  });

  /** index.css 里选中列表档基础态 `.nd-card` 的规则——预期恰好一条（按选择器过滤，不掺 body 内容，
   *  否则覆盖层之后再插一条「内容上不含 border-color 子串」的同选择器规则不会被发现，见头注变异靶）。*/
  const listViewBaseOverride = () => {
    const hits = flat(indexCss).filter((r) => r.sel === '#s-nodes.nodes-list-view .nd-card');
    expect(
      hits.length,
      'index.css 里 #s-nodes.nodes-list-view .nd-card 规则不是恰好一条——多出的那条无论内容是什么，' +
        '层叠里更晚出现的都会赢，可能悄悄覆盖掉这条的 border-color',
    ).toBe(1);
    return hits[0];
  };
  const listViewLastChildOverride = () => {
    const hits = flat(indexCss).filter(
      (r) => r.sel === '#s-nodes.nodes-list-view .nd-card:last-child',
    );
    expect(
      hits.length,
      'index.css 里 #s-nodes.nodes-list-view .nd-card:last-child 规则不是恰好一条',
    ).toBe(1);
    return hits[0];
  };
  /** 规则体内不得再出现 `border`/`border-bottom` 简写——简写会把未提到的 longhand 复位成
   *  currentcolor，即使写在这两行**后面**也会因为同规则内后声明胜出而赢，颜色又漏回 currentcolor。
   *  正则只匹配裸的 `border:`/`border-bottom:`，不会误伤 `border-color:`/`border-bottom-color:`
   *  （`-bottom` 之后紧跟 `\s*:` 才算命中，`-color`/`-radius` 都在这一步失配）。*/
  const noBorderShorthand = /(^|;)\s*border(-bottom)?\s*:/;

  it('④ F1：列表档 border-color 覆盖同样落在全部 @import 之后', () => {
    const base = listViewBaseOverride();
    const last = listViewLastChildOverride();
    expect(base.body, '基础态覆盖里没有 border-color —— 规则本身找错了').toMatch(/border-color/);
    expect(last.body, ':last-child 覆盖里没有 border-bottom-color —— 规则本身找错了').toMatch(
      /border-bottom-color/,
    );
    expect(indexCss.indexOf(base.body), '基础态覆盖写在 @import 前面 = 空操作').toBeGreaterThan(
      lastImport,
    );
    expect(indexCss.indexOf(last.body), ':last-child 覆盖写在 @import 前面 = 空操作').toBeGreaterThan(
      lastImport,
    );
  });

  it('⑤ F1：列表档 border-color 全程不含 currentcolor（列表档基础态 + last-child 两态钉住 --line/--hair；' +
    '卡片档那态本就是 screens.css 的 border 简写直接写死 --line，不途经 currentcolor，见下一条）', () => {
    const base = listViewBaseOverride();
    const last = listViewLastChildOverride();
    expect(base.body).toMatch(/border-color\s*:\s*hsl\(var\(--line\)\)/);
    expect(base.body).toMatch(/border-bottom-color\s*:\s*hsl\(var\(--hair\)\)/);
    expect(last.body).toMatch(/border-bottom-color\s*:\s*hsl\(var\(--hair\)\)/);
    expect(
      base.body,
      '不得含 currentcolor —— 一旦出现，border-color 又会在过渡里途经它，边框闪光复发',
    ).not.toMatch(/currentcolor/);
    expect(last.body, '不得含 currentcolor').not.toMatch(/currentcolor/);
    // 变异 C：规则体内追加一行 `border:0;`——同规则内后声明胜出，颜色又漏回 currentcolor，
    // 且不影响以上任何一条既有断言（border-color: --line 那行字面量还在，只是不再生效）。
    expect(
      base.body,
      '基础态覆盖内出现了 border/border-bottom 简写——会把上面钉的颜色重新复位成 currentcolor',
    ).not.toMatch(noBorderShorthand);
    expect(
      last.body,
      ':last-child 覆盖内出现了 border/border-bottom 简写——同上',
    ).not.toMatch(noBorderShorthand);
  });

  it('⑥ 覆盖层的 transition 取值本身是收窄过的——不是「两处相等」就够，相等的对象也要对（挡「同步退回 all」）', () => {
    const value = extractTransition(ndCardTransitionOverride().body);
    expect(value, '覆盖层没解析出 transition 取值').toBeTruthy();
    const props = value!.split(',').map((seg) => seg.trim().split(/\s+/)[0]);
    // ⓒ 不含裸时长项——单独这一条就能挡住「两处同步改回 `.14s`」这类同步退回 all 的变异
    // （②的「两处相等」在这种变异下仍然成立：两处都等成了同一个错值）。
    for (const p of props) {
      expect(
        p,
        `解析出一项「${p}」不像属性名，很可能是 transition-property:all 的裸时长简写`,
      ).not.toMatch(/^\.?\d/);
    }
    // ⓐ 至少含 hover/cur 真正用到的两个——否则可能是解析器扫到了别的规则。
    expect(props, '缺 border-color —— hover/cur 的边框反馈会瞬切').toContain('border-color');
    expect(props, '缺 box-shadow —— hover/cur 的阴影反馈会瞬切').toContain('box-shadow');
    // ⓑ 与白名单逐项相等——多出的可能是重新纳入了几何属性，少的是悄悄丢了一个绘制层属性。
    const whitelist = [
      'border-color',
      'box-shadow',
      'background',
      'border-radius',
      'outline',
      'outline-offset',
    ];
    expect(
      [...props].sort(),
      '收窄清单变了：核对 screens.css:61 的判据（不参与盒模型的绘制层属性）逐条重扫',
    ).toEqual([...whitelist].sort());
  });

  it('前提自检：卡片档 .nd-card 的 border-color 本就直接写死 --line，不经过任何简写复位', () => {
    const cardRule = flat(screensCss).find(
      (r) => r.sel === '.nd-card' && /transition\s*:/.test(r.body),
    );
    expect(cardRule, 'screens.css 的 .nd-card 规则不见了').toBeTruthy();
    expect(cardRule!.body).toMatch(/border\s*:\s*1px\s+solid\s+hsl\(var\(--line\)\)/);
  });
});

describe('主题两档同步：tokens 四块键集对等 + 主题条件规则必须成对', () => {
  /**
   * 把「所有涉及风格处理的，需要同步对浅色主题一并处理」从口头纪律变成机制。
   *
   * 主应用此前**一条这类门都没有**。范式借自 `update-popup/popup-theme.test.ts:59/63`
   * （「浅色档与深色档变量键集完全一致 —— 漏一个 = 那个颜色浅色下仍是深色取值」+「注入缺席兜底
   * 也覆盖同一组键」）。**照抄的是判据不是结构**：update-popup 是 `:root`=深 + `[data-theme=light]`
   * 两块，主应用 tokens.css 是**四块** —— `:root`（浅色基线）/ `[data-theme='dark']` /
   * `[data-theme='light']` / `@media(prefers-color-scheme:dark)` 里的无属性腿。四块两两对齐才成立。
   *
   * ⑤ 是本仓自己的形态、update-popup 没有的一条：主应用的主题条件规则一律**成对**出现 ——
   *   `@media (prefers-color-scheme:X){ :root:not([data-theme=…]) SEL{…} }` 与
   *   `:root[data-theme="X"] SEL{…}`。少任一条腿就漏掉一半用户：uiTheme 支持
   *   'system' / 'dark' / 'light' 三选一，只写显式腿 = 跟随系统的用户拿不到，只写 media 腿 =
   *   显式切换的用户拿不到。这正是 `.toast` 那次缺陷（本文件 :54 那道门）的**通用形态**，
   *   之前只按 `.toast` 一处钉过。
   *
   * 变异靶（实跑过）：
   *  · tokens.css 的 `[data-theme='light']` 删掉任一变量 ⇒ ② 红；
   *  · 改 `[data-theme='light']` 里某个变量的值、不改 `:root` 基线 ⇒ ③ 红；
   *  · 改 media 深色腿里某个变量的值、不改 `[data-theme='dark']` ⇒ ④ 红；
   *  · 在 index.css 加一条只有 `[data-theme="dark"]` 腿的规则 ⇒ ⑤ 红；
   *  · 在 index.css 写一个 `#rrggbb` / `rgba()` / 裸 `hsl(120 50% 50%)` ⇒ ⑥ 红；
   *  · 把 media 腿的 `:not([data-theme='dark'])` 去掉（只剩单 `:not`）⇒ ① 红。
   */
  const tokensRaw = stripComments(read('./tokens.css'));

  /** 抽 `@media (prefers-color-scheme: X)` 块 —— 按大括号配平，不用正则（内层还有一层块）。 */
  const mediaBlocks = (css: string) => {
    const out: { tone: string; inner: string; start: number; end: number }[] = [];
    const re = /@media\s*\(prefers-color-scheme\s*:\s*(dark|light)\)\s*\{/g;
    for (let m = re.exec(css); m; m = re.exec(css)) {
      let depth = 1;
      let i = m.index + m[0].length;
      for (; i < css.length && depth > 0; i++) depth += +(css[i] === '{') - +(css[i] === '}');
      out.push({ tone: m[1], inner: css.slice(m.index + m[0].length, i - 1), start: m.index, end: i });
    }
    return out;
  };
  /** 去掉全部 @media 块后的顶层文本。 */
  const outsideMedia = (css: string) => {
    let last = 0;
    const parts: string[] = [];
    for (const b of mediaBlocks(css)) {
      parts.push(css.slice(last, b.start));
      last = b.end;
    }
    parts.push(css.slice(last));
    return parts.join('');
  };
  const flatRules = (css: string) =>
    [...css.matchAll(/([^{}]+)\{([^}]*)\}/g)].map((m) => ({ sel: m[1].trim(), body: m[2] }));
  /** 声明体 → `--x` → 归一化取值。 */
  const varsOf = (body: string) =>
    new Map(
      [...body.matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)].map(
        (m) => [m[1], m[2].replace(/\s+/g, ' ').trim()] as const
      )
    );

  /**
   * tokens.css 的四个档。`:root` 有两块（颜色块 + 几何块），按含不含 `--bg` 认。
   *
   * **懒求值 + 抛 Error（不是 describe 体里直接 expect）**：describe 体在 vitest 的 collect 阶段跑，
   * 那里断言失败会让**整个文件**变成「no tests」—— 本文件另外 9 个 describe 会一起消失，看上去像
   * 「没跑」而不是「红了」，这是最坏的一种门失效。抛出的 Error 落在 `it` 里就是一条普通失败。
   */
  const readBands = (() => {
    let cache: Record<'lightBase' | 'darkExplicit' | 'lightExplicit' | 'darkMedia', Map<string, string>> | null =
      null;
    return () => {
      if (cache) return cache;
      const topLevel = flatRules(outsideMedia(tokensRaw));
      const pickRoot = (sel: string) => {
        const hit = topLevel.filter((r) => r.sel === sel && /--bg\s*:/.test(r.body));
        if (hit.length !== 1)
          throw new Error(`tokens.css 里 ${sel} 的颜色块不是恰好一个（找到 ${hit.length} 个）`);
        return varsOf(hit[0].body);
      };
      const blocks = mediaBlocks(tokensRaw).filter((b) => b.tone === 'dark');
      if (blocks.length !== 1) throw new Error('tokens.css 的 prefers-color-scheme:dark 块不是恰好一个');
      const inner = flatRules(blocks[0].inner).filter((r) => /--bg\s*:/.test(r.body));
      if (inner.length !== 1) throw new Error('dark media 腿里的变量块不是恰好一个');
      if (!/:root:not\(\[data-theme='light'\]\):not\(\[data-theme='dark'\]\)/.test(inner[0].sel))
        throw new Error(
          `dark media 腿的选择器变了（现为 ${inner[0].sel}）—— 少一个 :not 就会让**显式切浅色**的用户` +
            '在系统偏好为深色时被这条腿污染成深色取值'
        );
      cache = {
        lightBase: pickRoot(':root'),
        darkExplicit: pickRoot(":root[data-theme='dark']"),
        lightExplicit: pickRoot(":root[data-theme='light']"),
        darkMedia: varsOf(inner[0].body),
      };
      return cache;
    };
  })();

  const keys = (m: Map<string, string>) => [...m.keys()].sort();

  it('① 自检：四个档都读到了、选择器没变形、且非空（断言不是在空集上恒绿）', () => {
    const b = readBands();
    for (const [name, m] of [
      [':root 浅色基线', b.lightBase],
      ["[data-theme='dark']", b.darkExplicit],
      ["[data-theme='light']", b.lightExplicit],
      ['@media dark 无属性腿', b.darkMedia],
    ] as const) {
      expect(m.size, `${name} 是空的`).toBeGreaterThan(20);
      expect(m.has('--surface'), `${name} 少了 --surface`).toBe(true);
    }
  });

  it('② 四个档的变量键集完全一致（漏一个 = 那个颜色在另一档下仍取旧值）', () => {
    const { lightBase, darkExplicit, lightExplicit, darkMedia } = readBands();
    const base = keys(darkExplicit);
    expect(keys(lightBase), ':root 浅色基线与深色档键集不一致').toEqual(base);
    expect(keys(lightExplicit), "[data-theme='light'] 与深色档键集不一致").toEqual(base);
    expect(keys(darkMedia), '@media 无属性腿与深色档键集不一致').toEqual(base);
  });

  it('③ 两个浅色档**逐值**一致（`:root` 基线 vs 显式 light —— 改一个漏一个 = 切换主题变色）', () => {
    const { lightBase, lightExplicit } = readBands();
    const diff = keys(lightBase).filter((k) => lightBase.get(k) !== lightExplicit.get(k));
    expect(
      diff.map((k) => `${k}: :root=${lightBase.get(k)} / light=${lightExplicit.get(k)}`),
      '显式切「浅色」与跟随系统浅色不同色 —— 同一个主题两种取值'
    ).toEqual([]);
  });

  it('④ 两个深色档**逐值**一致（显式 dark vs @media 无属性腿）', () => {
    const { darkExplicit, darkMedia } = readBands();
    const diff = keys(darkExplicit).filter((k) => darkExplicit.get(k) !== darkMedia.get(k));
    expect(
      diff.map((k) => `${k}: dark=${darkExplicit.get(k)} / media=${darkMedia.get(k)}`),
      '显式切「深色」与跟随系统深色不同色'
    ).toEqual([]);
  });

  it('⑤ 主题条件规则必须成对：media 腿与显式 [data-theme] 腿一一对应', () => {
    /**
     * 已登记的历史缺腿（**只此一条**；再多一条本断言即红）：
     *   prototype.css `:root[data-theme="light"] .stage` 有显式腿、无 media 腿。
     * 不修的判据（已核实，非「懒得动」）：① prototype.css 是原型逐字镜像的只读区；
     * ② 该背景在真实 app 里**画不出来** —— 原型的 `.stage` 是浮窗卡背后的桌面底，真实窗口里
     * `.win` 被覆盖层拉成 `width:100%;height:100vh` 且底为不透明 `--surface`，把 `.stage` 整个盖住；
     * mac 特效模式下 index.css 又把它强制 `transparent`。即这是**死漆**，不是用户可见的半修。
     */
    const KNOWN_HALF_LEG = ['prototype.css light 只有显式腿：.stage'] as const;

    const normSel = (sel: string) =>
      sel
        .split(',')
        .map((p) =>
          p
            .trim()
            .replace(/^:root(\[data-theme=['"][a-z]+['"]\]|:not\(\[data-theme=['"][a-z]+['"]\]\))*/, '')
            .trim()
        )
        .sort()
        .join(' , ');
    const normBody = (b: string) => b.replace(/\s+/g, ' ').trim().replace(/;$/, '');

    const gaps: string[] = [];
    for (const file of ['./index.css', './components.css', './screens.css', './prototype.css'] as const) {
      const css = stripComments(read(file));
      const media = new Map<string, Set<string>>();
      for (const b of mediaBlocks(css)) {
        for (const r of flatRules(b.inner)) {
          const s = normSel(r.sel);
          if (!s.replace(/ , /g, '')) continue; // 纯变量块（`:root{--x:…}`）不参与，见 ①–④
          media.set(b.tone, (media.get(b.tone) ?? new Set()).add(`${s}|${normBody(r.body)}`));
        }
      }
      const explicit = new Map<string, Set<string>>();
      for (const r of flatRules(outsideMedia(css))) {
        const tone = r.sel.match(/:root\[data-theme=['"](dark|light)['"]\]/)?.[1];
        if (!tone) continue;
        const s = normSel(r.sel);
        if (!s.replace(/ , /g, '')) continue;
        explicit.set(tone, (explicit.get(tone) ?? new Set()).add(`${s}|${normBody(r.body)}`));
      }
      for (const tone of ['dark', 'light'] as const) {
        const m = media.get(tone) ?? new Set<string>();
        const e = explicit.get(tone) ?? new Set<string>();
        const base = file.replace('./', '');
        for (const x of [...m].filter((v) => !e.has(v)))
          gaps.push(`${base} ${tone} 只有 media 腿：${x.split('|')[0]}`);
        for (const x of [...e].filter((v) => !m.has(v)))
          gaps.push(`${base} ${tone} 只有显式腿：${x.split('|')[0]}`);
      }
    }
    expect(gaps.length, '一条主题条件规则都没扫到 —— 走查逻辑失效了（恒绿）').toBeGreaterThanOrEqual(0);
    expect(
      gaps.sort(),
      '主题条件规则缺腿：只写显式 [data-theme] 腿 ⇒ 跟随系统的用户拿不到；只写 @media 腿 ⇒ ' +
        '显式切主题的用户拿不到。两条腿必须同值同时给。'
    ).toEqual([...KNOWN_HALF_LEG]);
  });

  it('⑥ 覆盖层 index.css 里不得出现裸色值（新颜色一律进 tokens，否则天然只有一档）', () => {
    // 判据同 popup-theme.test.ts:74「白/黑半透明覆盖层只许出现在变量声明里」，这里推广到
    // 全部裸色写法：`#rgb(a)` / `rgb()` / `rgba()` / 裸 `hsl(<数字> …)`。`hsl(var(--x))` 不算。
    const css = stripComments(read('./index.css'));
    const offenders = [
      ...css.matchAll(/#[0-9a-fA-F]{3,8}\b|\brgba?\(|\bhsla?\(\s*[\d.]/g),
    ].map((m) => m[0]);
    expect(
      offenders,
      'index.css 出现裸色值 —— 它只在当前一档成立，另一档必然错。请落成 tokens.css 的变量。'
    ).toEqual([]);
  });
});

describe('浅色 --err 的文字对比度：本仓实际用到的每一种底色都要过 WCAG AA 4.5:1', () => {
  /**
   * 被守的缺陷：浅色 `--err` 原值 `356 68% 50%` 的 L*=47.2，**落在纯白上也只有 4.97** ——
   * 那已经是天花板，底色但凡染一点色就掉到 4.5 以下。实测（`ui/dist` 产物 CSS + 真 Chromium
   * `computedStyle` 取层叠赢家 + WCAG/CIE L*）改前有 **18 处**不过 AA，最低 3.84
   * （`.csel-trigger.danger:hover`，底 `--surface-3`），报上来的状态栏/csel 触发器 4.33 只排中游。
   * 修法与逐条数字见 `index.css` 顶部「浅色 --err 下调」段。
   *
   * 这道门为什么是**算术**而不是「盯住某个字面量」：真正要守的不是「--err 等于某个值」，而是
   * 「err 文字落在本仓实际用到的每一种底色上都够看」。底色任一改动（`--surface-3` 调暗、淡染从
   * 0.15 加到 0.25、新增一种 err 文字的落底）都该让这道门红，而不是等真机上被人看出来。
   * 算术的正确性由 ⓪ 对着真 Chromium 的实测值自校 —— 不是自己算自己。
   *
   * **射程边界（别误读）**：本门只管**浅色档**、只管 `--err` 当**文字**。
   *  · 深色档有两处实测不过且**修法方向相反**（`.btn.danger` 白字落 err 实底 3.16；
   *    `.csel-trigger.danger:hover` 4.26）—— 调 `--err` 救一个必压另一个，得改规则，另开一轮；
   *  · `--err` 当色点/描边（`.dot.err` / `--err/0.3` 边框等）走 WCAG 1.4.11 的 3:1 档，不在本门；
   *  · 同屏的 `--warn` 落状态栏实测只有 **3.19**、`--ok` 3.38 —— 比本门修的还差，但属另一个 token。
   *
   * 变异靶（**逐个实跑过**，每次改完跑门再从副本还原 + sha256 校验）：
   *  · index.css 覆盖值改回 `356 68% 50%`            ⇒ ③④ 红
   *  · 删掉 index.css 整段覆盖                        ⇒ ①②③④⑤ 全红
   *  · 覆盖挪到全部 `@import` 之前（= 空操作）        ⇒ ① 红（并带红上面「主题条件规则成对」那条）
   *  · 覆盖选择器加一条 `:root[data-theme='dark']`    ⇒ ② 红
   *  · 只改 index.css 不改 tokens.css                 ⇒ ③ 红（并带红「两个浅色档逐值一致」）
   *  · prototype.css `--surface-3` 调暗到 `210 22% 85%` ⇒ ④ 红（底色变了，实测结论要重做）
   *  · `.act-block` 淡染 `--err/0.15` → `/0.3`        ⇒ ④ 红（淡染是扫出来的，新档自动进表）
   *  · 把 `lum()` 的蓝通道系数改坏                     ⇒ ⓪ 红（算术回归自己会自曝）
   *
   * **抓不到什么**（写清楚，别当成全覆盖）：
   *  · 深色档 —— 一条都不管（射程见上）；
   *  · 新增一个 err 文字落点、其底色是本花名册**没有**的第七种面 —— 花名册是人工登记的，
   *    只有淡染那一类是扫出来的自动项；
   *  · 字号/字重（AA 对 ≥18.66px bold / ≥24px 放宽到 3:1，本门一律按 4.5 判，是**偏严**不是偏松）；
   *  · 真机观感（红得是否过深、与同屏 `--warn` 琥珀是否失衡）—— 只有真机能定。
   */
  const indexCss = stripComments(read('./index.css'));
  const tokensCss = stripComments(read('./tokens.css'));
  /** 扫「err 文字的底色」时要覆盖的全部样式文件（禁区文件也在内 —— 它们才是大多数落点）。 */
  const STYLE_FILES = ['./index.css', './components.css', './screens.css', './prototype.css'] as const;

  /** index.css 里那条浅色 `--err` 覆盖（`:root, :root[data-theme='light']`）。 */
  const overrideRule = () => {
    const hits = flat(indexCss).filter(
      (r) => /(^|,\s*):root(\s*,|$)/.test(r.sel) && readVar(r.body, '--err')
    );
    if (hits.length !== 1)
      throw new Error(`index.css 里带 --err 的 :root 覆盖不是恰好一条（找到 ${hits.length} 条）`);
    return hits[0];
  };

  it('⓪ 自检：本文件的对比度算术能复现真 Chromium 的实测值（否则下面的数全是自说自话）', () => {
    const old = hslToRgb('356 68% 50%');
    expect(contrast(old, hslToRgb('210 28% 94%')), '旧 err 落 --surface-2：实测 4.33').toBe(4.33);
    expect(contrast(old, hslToRgb('210 22% 89%')), '旧 err 落 --surface-3：实测 3.84').toBe(3.84);
    expect(contrast(old, hslToRgb('0 0% 100%')), '旧 err 落 --surface：实测 4.97').toBe(4.97);
    expect(
      contrast(old, over(old, 0.15, hslToRgb('0 0% 100%'))),
      '旧 err 落 err/0.15 淡染：实测 3.94'
    ).toBe(3.94);
    expect(contrast(hslToRgb('356 68% 44%'), hslToRgb('210 28% 94%')), '新 err 落 --surface-2：实测 5.34').toBe(5.34);
  });

  it('① 覆盖必须在 index.css 里、且落在最后一个 @import 之后（写在前面会被 prototype.css 反压）', () => {
    const rule = overrideRule();
    const lastImport = indexCss.lastIndexOf('@import');
    expect(lastImport, 'index.css 里一个 @import 都没有 —— 走查逻辑失效了').toBeGreaterThan(0);
    expect(
      indexCss.indexOf(rule.body),
      'tokens.css 的浅色取值是死代码（prototype.css 是最后一个 @import 且自带同选择器令牌块）：' +
        '浅色 --err 的覆盖必须写在全部 @import 之后，写在前面 = 空操作。'
    ).toBeGreaterThan(lastImport);
  });

  it('② 覆盖只挂浅色选择器（挂上深色 = 把深色也改了，而深色需要的是相反方向）', () => {
    const parts = overrideRule()
      .sel.split(',')
      .map((s) => s.trim());
    expect(parts.sort(), '浅色 --err 覆盖的选择器变了').toEqual([':root', ":root[data-theme='light']"]);
  });

  it('③ tokens.css 的两条浅色腿与覆盖同值（真值源不许说谎：改一处漏一处 = 读代码的人被骗）', () => {
    const value = readVar(overrideRule().body, '--err');
    const legs = flat(tokensCss).filter(
      (r) => (r.sel === ':root' || r.sel === ":root[data-theme='light']") && readVar(r.body, '--bg')
    );
    expect(legs.length, 'tokens.css 的两个浅色档没找齐').toBe(2);
    for (const leg of legs)
      expect(
        readVar(leg.body, '--err'),
        `tokens.css ${leg.sel} 的 --err 与 index.css 的生效覆盖不同值 —— tokens.css 那份进不了浏览器，` +
          '留着旧值只会骗下一个改这里的人'
      ).toBe(value);
  });

  it('④ 浅色 err 文字落在每一种实际底色上都 ≥4.5:1', () => {
    const results = backdrops().map(({ what, rgb }) => ({
      what,
      ratio: contrast(effectiveErr(), rgb),
    }));
    expect(
      results.filter((r) => r.ratio < 4.5).map((r) => `${r.what} = ${r.ratio}`),
      '浅色 err 文字有底色不过 WCAG AA 4.5:1。改 --err 的明度（根因），别给单处另开 token。'
    ).toEqual([]);
  });

  it('⑤ 自检：花名册非空，且拿改前的 356 68% 50% 跑同一张表必须有不过的（证明这张表有牙）', () => {
    const list = backdrops();
    expect(list.length, '底色花名册是空的 —— ④ 在空集上恒绿').toBeGreaterThanOrEqual(8);
    const old = hslToRgb('356 68% 50%');
    expect(
      list.filter((b) => contrast(old, b.rgb) < 4.5).length,
      '用改前的 --err 跑，这张表居然一条都不红 —— 花名册漏了本次修的那些底色'
    ).toBeGreaterThanOrEqual(6);
  });

  /** 生效的浅色 `--err`（= index.css 的覆盖，不是 tokens.css 那份死代码）。 */
  function effectiveErr(): RGB {
    return hslToRgb(readVar(overrideRule().body, '--err')!);
  }

  /**
   * 「err 文字落在什么底上」的花名册 = 不透明面 token（值从 prototype.css 的浅色 `:root` 现读，
   * 底色一改这里跟着变）+ `--err` 自身的淡染背景（alpha 从四个样式文件里**扫**出来，
   * 新增一档淡染会自动进表）。淡染一律按叠在 `--surface` 上算 —— 实测里这些淡染的父面都是白卡片。
   */
  function backdrops(): { what: string; rgb: RGB }[] {
    const protoLight = flat(stripComments(read('./prototype.css'))).find(
      (r) => r.sel === ':root' && readVar(r.body, '--bg')
    );
    if (!protoLight) throw new Error('prototype.css 的浅色 :root 令牌块没找到');
    const tok = (name: string) => {
      const v = readVar(protoLight.body, name);
      if (!v) throw new Error(`prototype.css 浅色块里没有 ${name}`);
      return hslToRgb(v);
    };
    const surface = tok('--surface');
    // 不透明面：每条后面是实测到的落点，不是凭空列的
    const out: { what: string; rgb: RGB }[] = [
      { what: '--surface（.err-line / .log-ERROR / .csel-opt.danger / 卡片内全部 err 文字）', rgb: surface },
      { what: '--surface-2（#sb-lat.dead 状态栏 / .csel-trigger.danger idle）', rgb: tok('--surface-2') },
      { what: '--surface-3（.csel-trigger.danger:hover —— 全仓最差的一处）', rgb: tok('--surface-3') },
      { what: '--bg（卡片外的屏幕底）', rgb: tok('--bg') },
      { what: '--err-weak（.pill.err / .pending-bar.err / 五处 hover 危险菜单项）', rgb: tok('--err-weak') },
      { what: '--flow-weak（.aad-sel-chip:hover）', rgb: tok('--flow-weak') },
    ];
    const alphas = new Set<string>();
    for (const f of STYLE_FILES)
      for (const m of stripComments(read(f)).matchAll(
        /background\s*:\s*hsl\(\s*var\(--err\)\s*\/\s*([\d.]+)\s*\)/g
      ))
        alphas.add(m[1]);
    if (alphas.size === 0) throw new Error('一处 --err 淡染背景都没扫到 —— 扫描逻辑失效了');
    const err = effectiveErr();
    for (const a of [...alphas].sort())
      out.push({ what: `--err/${a} 淡染叠在 --surface 上（.act-block / .nd-lat.dead / .dlg-err 一族）`, rgb: over(err, +a, surface) });
    return out;
  }
});

describe('实体色按钮禁用态 + 资源库下载按钮的宽度稳定性（2026-07-30 mac 真机）', () => {
  /**
   * 真机原话：「下载选中会置灰，但是跟取消按钮中间会残留上一次亮起的实体蓝色 + 下 字」。
   * 拆出来是两个独立缺陷，各自钉一组：
   *
   * **A 类（全仓）**：`.btn:disabled` 只有 `opacity:.5`（components.css:54 / prototype.css:300，
   *   两份逐字重复且都是禁区文件），**不动背景**。于是 `.btn.flow` 禁用后是「50% 透明的品牌蓝」——
   *   仍是彩色实底，正是用户读成「还亮着」的原因。覆盖落 index.css（见该文件同名段的完整权衡）。
   *
   * **B 类（此一处）**：页脚计数 span 原为 `downloadTargets.length > 0 && <span>…</span>` 条件渲染
   *   ⇒ 勾选↔取消勾选之间按钮宽度 78.906px ↔ 53.016px（实测），而 `.dlg-foot` 是
   *   `justify-content:flex-end` ⇒ 右边界钉死、**左边界横移 25.89px**。让出来的那条竖条若不被重绘，
   *   留下的正是旧按钮左半截 =「实体蓝 + 首字下」。改为**恒渲染**（含 `(0)`）后，0~9 选中项宽度
   *   逐像素相同（五语实测一致）⇒ 报上来的那条路径不再有几何变化。
   *   ⚠️ 本机 WebKitGTK 4.1 **未能复现**那条残留（终态帧与「直接以终态首屏渲染」的帧逐像素全等），
   *   故 B 类是**防御性**修法，真机复验前不得当作已证实 —— 但「宽度不再变」这个结构事实可断言，
   *   本组钉的就是它，不是钉「残留没了」。
   *
   * 变异靶（**实跑过**，见交接）：
   *  · 删掉 index.css 那条覆盖规则 ⇒ ②③ 红；
   *  · 把覆盖规则的 `background` 换回 `hsl(var(--flow))` ⇒ ② 红；
   *  · 删掉覆盖规则里的 `opacity:1` ⇒ ③ 红；
   *  · 去掉选择器打头的 `:root`（「顺手清理冗余」最容易动的一处）⇒ ④ 红；
   *  · 把计数 span 改回 `downloadTargets.length > 0 &&` 条件渲染 ⇒ ⑥ 红；
   *  · 把键名改回 `resCatalog.downloadSelected` / 把译文改回「下载选中」⇒ ⑦⑧ 红。
   */
  const LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'] as const;
  const indexCss = stripComments(read('./index.css'));
  const catalog = stripComments(read('../components/dialogs/ResCatalogDialog.tsx'));

  /** index.css 里所有命中 `.btn.flow:disabled` 的顶层规则（选择器 + 归一化声明体）。 */
  const solidDisabled = [...indexCss.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .map((m) => ({ sel: m[1].trim().replace(/\s+/g, ' '), body: m[2].replace(/\s+/g, ' ').trim() }))
    .filter((r) => /\.btn\.flow:disabled/.test(r.sel));

  it('① 自检：覆盖规则恰好一条且声明体非空（解析失效会让下面几条恒绿）', () => {
    expect(
      solidDisabled.map((r) => r.sel),
      'index.css 里 `.btn.flow:disabled` 的覆盖规则不是恰好一条'
    ).toHaveLength(1);
    expect(solidDisabled[0].body.length, '声明体是空的 ⇒ 断言无意义').toBeGreaterThan(20);
  });

  it('② 实体色按钮禁用后不得仍是实体品牌色（`.flow` 与 `.danger` 都要覆盖）', () => {
    const { sel, body } = solidDisabled[0];
    expect(sel, '`.btn.danger:disabled` 没一起覆盖 ⇒ 实体红按钮禁用后照旧是红实底').toContain(
      '.btn.danger:disabled'
    );
    const bg = /background\s*:\s*([^;]+)/.exec(body)?.[1] ?? '';
    expect(bg, '禁用态没有显式换背景 ⇒ 只压不透明度，缺陷原样').not.toBe('');
    expect(
      bg,
      `禁用态背景仍引用品牌色令牌（${bg}）—— 「不可用」却仍是实体品牌面，正是真机读成「还亮着」的根因`
    ).not.toMatch(/--flow|--err/);
  });

  it('③ `opacity` 必须显式回 1（否则与 `.btn:disabled{opacity:.5}` 叠加，标签对比度掉到 ~1.9）', () => {
    // 「换中性底」与「压不透明度」是两套禁用语汇，叠加会互相抵消：实测叠加后标签只剩 1.84~2.64，
    // **比不改还糟**。所以换底色的那条必须把 opacity 收回来，二者只能取一。
    expect(
      solidDisabled[0].body,
      'opacity 没显式回 1 ⇒ 中性底被 .5 压进容器色，标签几乎读不出'
    ).toMatch(/opacity\s*:\s*1\b/);
  });

  it('④ 选择器必须带 `:root` 前缀 —— 特异性刚好卡在浅色档，去掉即浅色静默回退', () => {
    // 浅色档的 `:root[data-theme="light"] .btn.flow`（prototype.css:1290）与其 media 腿
    // `:root:not([data-theme="dark"]) .btn.flow`（:1285）都是 (0,4,0)；`.btn.flow:disabled`
    // 只有 (0,3,0)。少了 `:root` 就**赢不了浅色档**，深色下看着好好的、浅色下还是蓝底 ——
    // 这是「顺手删掉看起来冗余的 :root」必然踩的坑，故单独钉一条。
    for (const part of solidDisabled[0].sel.split(','))
      expect(
        part.trim(),
        `覆盖规则的这一支没有 :root 前缀（${part.trim()}）⇒ 特异性 (0,3,0) < 浅色档的 (0,4,0)`
      ).toMatch(/^:root\s/);
  });

  it('⑤ 覆盖只针对实体变体，不得外溢到 `.btn.ghost` 与裸 `.btn`', () => {
    // 后两者本就没有实底，`opacity:.5` 已经读得出禁用；给它们换底色会造出
    // 「禁用比可用还显眼」的新问题。射程按「实体品牌色」这一类收口。
    for (const part of solidDisabled[0].sel.split(',')) {
      expect(part, `覆盖外溢到裸 .btn:disabled（${part.trim()}）`).not.toMatch(
        /\.btn:disabled/
      );
      expect(part, `覆盖外溢到 .btn.ghost（${part.trim()}）`).not.toMatch(/\.ghost/);
    }
  });

  it('⑥ 资源库页脚计数**恒渲染** —— 回到条件渲染即恢复 25.89px 的宽度跳变', () => {
    expect(
      catalog,
      '计数 span 改回了条件渲染（`downloadTargets.length > 0 &&`）⇒ 勾选↔取消勾选按钮变宽变窄，' +
        '`.dlg-foot` 右对齐 ⇒ 左边界横移，真机残留缺陷的触发条件被装回来了'
    ).not.toMatch(/downloadTargets\.length\s*>\s*0\s*&&/);
    expect(catalog, '计数 span 整个不见了 ⇒ 断言退化成恒绿').toMatch(
      /<span>&nbsp;\(\{downloadTargets\.length\}\)<\/span>/
    );
  });

  it('⑦ 页脚按钮文案键为 `resCatalog.download`，旧键 `downloadSelected` 不得复活', () => {
    expect(catalog, 'ResCatalogDialog 仍在用旧键 `resCatalog.downloadSelected`').not.toContain(
      'downloadSelected'
    );
    expect(catalog, '页脚按钮没引用 `resCatalog.download`').toContain("t('resCatalog.download'");
  });

  it('⑧ 五语都有 `resCatalog.download`，且中英三语文案不得再含「选中」语义', () => {
    // 真机原话「下载选中应该改成下载，不需要选中二次」：选中数由紧随其后的 `(N)` 表达，
    // 标题里再写一次「选中」是同义重复。ru/fa 只断言键在（无对应词形，不钉字面）。
    const FORBIDDEN: Partial<Record<(typeof LOCALES)[number], RegExp>> = {
      'zh-CN': /选中/,
      'zh-TW': /所選|選中/,
      'en-US': /selected/i,
    };
    for (const l of LOCALES) {
      const json = JSON.parse(read(`../i18n/locales/${l}.json`));
      const v = json.resCatalog?.download;
      expect(v, `${l} 缺 resCatalog.download（改键时漏补这一语）`).toBeTypeOf('string');
      expect(json.resCatalog?.downloadSelected, `${l} 残留死键 resCatalog.downloadSelected`).toBeUndefined();
      const bad = FORBIDDEN[l];
      if (bad) expect(v, `${l} 文案「${v}」仍含「选中」语义 —— 与紧随其后的 (N) 同义重复`).not.toMatch(bad);
    }
  });
});

describe('浅色 --warn / --ok / --dn 的文字对比度，与深色 .btn.danger 的墨色', () => {
  /**
   * 被守的缺陷（2026-07-31）：
   *  · 浅色 `--warn` / `--ok` / `--dn` 原值落在正文上**一处都不过 AA** —— 状态栏实测
   *    warn 3.19 / ok 3.38 / dn 3.16，最差的 `.nd-top .nd-lat.slow`（warn/0.14 淡染叠在列表卡
   *    hover 底上）只有 **2.91**。
   *  · 深色 `.btn.danger` 是白字压 `--err` 亮红实底 = **3.16**。
   * 修法与逐值推导见 `index.css` 的「浅色语义色下调」段与「深色 .btn.danger 换墨色」段。
   *
   * **门槛为什么一律 4.5（查过用法才定的，不是默认值）**：这三个 token 当文字的落点全部是
   * 10~12.5px（`.pill` 10.5 / `.nd-cap` 10 / `.warn-line` 11 / `.statusbar` 11.5 / `.log-view` 12 /
   * `.core-ver-banner .cvb-tx b` 12.5 …），`.btn.danger` 是 12.5px/660 —— 没有一处够得上 AA 的
   * 大字例外（≥18.66px bold 或 ≥24px）。它们当**色点/描边/进度条/开关轨/图标**的那些用法
   * （`.dot.*` `.lat-*` `.swt.indet` `.sub-usage .bar>i` `.pb-ic`/`.cvb-ic` 的 13px svg、
   * `.connect-btn.busy` 的 22px svg）走 WCAG 1.4.11 的 3:1，**不在本门**。
   *
   * 花名册怎么来的（与浅色 `--err` 那道门的口径差异，别当成抄的）：
   *  · 不透明面（`--surface` / `--surface-2` / `--bg` / `--<tok>-weak`）从 `prototype.css` 的
   *    当档令牌块**现读**，底色一改这里跟着变；
   *  · 自淡染（`--warn/0.14` 之类）不写死，从四个样式文件里**扫**：只认「同一条规则里既有
   *    `color:hsl(var(--tok))` 又有 `background:hsl(var(--tok)/α)`」的那种 —— 这样
   *    `.log-view mark`（`color:inherit`）与 `.pb-ic`（`color:#241400`）这些「底是该色、字不是该色」
   *    的规则不会被误收进来（收进来就成了「要求该色压在自己身上可读」，永远不可能过）；
   *  · **淡染要按 hover 后的行底算**：`.act-*` / `.nd-lat.*` 这类 chip 的父行在 hover 时会变成
   *    `--surface-2/0.45~0.55`，同一块 chip 在 hover 帧比静止帧暗一档。α 从样式文件里扫，取最大
   *    （最暗）那一档 ⇒ 将来谁把行 hover 调暗，本门自动跟着变严。浅色 `--err` 那道门当时只按
   *    「叠在 --surface 上」算，漏了这一层（它现在的最差项 4.46 就在这层上）—— 那属另一个 token，
   *    见交接，**不在本门射程**，别顺手扩进来。
   *
   * 变异靶（12 个**逐个实跑过**，下面写的是**实际观测到的**红项，不是预期；每次跑完从副本还原并
   * `sha256sum -c` 校验，不用 git 恢复类命令）：
   *  · index.css 删掉 `--warn` 那一行           ⇒ ①②③④⑦⑧ 红（本组 6 条；⑥ 不红 —— 它跑的是旧值，
   *                                               不读覆盖块，这正是它作为「花名册验牙」该有的独立性）
   *  · `--warn` 调回 `32 84% 42%`               ⇒ ③④ 红
   *  · `--ok` 只回一档到 `152 60% 28%`          ⇒ ③④ 红（那一档最差 4.46，肉眼看不出、门看得出）
   *  · 只改 index.css 不同步 tokens.css         ⇒ ③ 红（并带红「主题两档同步」③）
   *  · 覆盖块挪到 `@import` 之前（= 空操作）    ⇒ ① 红（并带红 --err 那道门的 ① 与「成对」⑤）
   *  · 覆盖块加一条 `:root[data-theme='dark']`  ⇒ ② 红（并带红 --err 那道门的 ②）
   *  · `.rule-item:hover` 0.55 → **0.9**        ⇒ ④ 红。**敏感度就到这里**：0.7 时最差还有 4.56、
   *                                               门是绿的，0.85 起才翻红 —— 别把本门当成「行底一动就报」。
   *  · 新增 `color:--ok` + `background:--ok/0.25` 的 chip ⇒ ④⑤ 红（淡染是扫出来的，浅深两档自动进表）
   *  · 深色 `.btn.danger` 删掉 media 腿          ⇒ ⑦ 红（并带红「主题条件规则成对」⑤）
   *  · 深色 `.btn.danger` 去掉 `:not(:disabled)` ⇒ ⑦ 红
   *  · `.hc-step.done .st-n` + `.log-view mark` 的墨色覆盖删掉 ⇒ ⑧ 红
   *  · 把 `lum()` 的蓝通道系数改坏               ⇒ ⓪④ 红（并带红 --err 那道门的 ⓪⑤）
   *
   * **抓不到什么**（写清楚，别当全覆盖）：
   *  · `--up` / `--flow-hi` / `--err` 在 hover 行底上的那三条（4.32 / 4.12 / 4.46）—— 本轮扫出来的
   *    新发现，不是本门射程，本门一条都不管；
   *  · `.btn.confirming` / `.nd-a.confirming` 深色白字压 err 实底（同样 3.16，带 `!important`）——
   *    刻意不修，判据见 index.css 该段末尾；
   *  · 字号/字重：本门一律按 4.5 判，是**偏严**不是偏松；
   *  · 新增一处该色文字、落在花名册没有的第七种不透明面上 —— 不透明面是人工登记的，只有淡染与
   *    行 hover α 是扫出来的自动项；
   *  · 真机观感（褐琥珀是否可接受、三色明度拉平后语义是否仍好辨）—— 只有真机能定。
   */
  const STYLE_FILES = ['./index.css', './components.css', './screens.css', './prototype.css'] as const;
  const indexCss = stripComments(read('./index.css'));
  const tokensCss = stripComments(read('./tokens.css'));
  const protoCss = stripComments(read('./prototype.css'));

  /** 被守的三个 token。`before` = 原型旧值，只用来给花名册验牙（⑥）。 */
  const TOKENS = {
    '--warn': { before: '32 84% 42%', weak: '--warn-weak' },
    '--ok': { before: '152 60% 36%', weak: '--ok-weak' },
    '--dn': { before: '197 80% 42%', weak: null },
  } as const;
  type Tok = keyof typeof TOKENS;
  const TOK_NAMES = Object.keys(TOKENS) as Tok[];

  const must = <T>(v: T | undefined | null, what: string): T => {
    if (v === undefined || v === null) throw new Error(`${what} —— 走查逻辑失效了，别让门静默变绿`);
    return v;
  };

  /** index.css 里那条浅色令牌覆盖（与 `--err` 同一块）。 */
  const overrideRule = () => {
    const hits = flat(indexCss).filter(
      (r) => /(^|,\s*):root(\s*,|$)/.test(r.sel) && readVar(r.body, '--warn')
    );
    if (hits.length !== 1)
      throw new Error(`index.css 里带 --warn 的 :root 覆盖不是恰好一条（找到 ${hits.length} 条）`);
    return hits[0];
  };

  /** prototype.css 的当档令牌块（浅色 `:root` / 深色 `[data-theme="dark"]`）。 */
  const protoBand = (band: 'light' | 'dark') =>
    must(
      flat(protoCss).find(
        (r) =>
          r.sel === (band === 'light' ? ':root' : ':root[data-theme="dark"]') && readVar(r.body, '--bg')
      ),
      `prototype.css 的${band === 'light' ? '浅' : '深'}色 :root 令牌块没找到`
    );

  /** 生效值：浅色取 index.css 的覆盖（tokens.css 那份是死代码），深色取 prototype 的深色块。 */
  const effective = (tok: Tok | '--err', band: 'light' | 'dark'): RGB =>
    hslToRgb(
      band === 'light'
        ? must(readVar(overrideRule().body, tok), `index.css 覆盖里没有 ${tok}`)
        : must(readVar(protoBand('dark').body, tok), `prototype.css 深色块里没有 ${tok}`)
    );

  /**
   * 带自淡染的 chip 实际挂在哪些「hover 会变底」的行/卡上 —— **人工登记**（判据同浅色 `--err` 那道门
   * 的不透明面花名册：宿主是人工的，α 是扫出来的）。不能改成「扫全部 `--surface-2/α` 取最大」：
   * 那会把 `.mesh-col:hover` 的 0.85 收进来，而 mesh 卡里根本没有这几种 chip ⇒ 凭空严一大截。
   */
  const TINT_HOSTS = [
    '.rule-item:hover', // .pill.act-direct / .act-block（RuleItem.tsx）
    '.ap-row:hover', // .act-direct / .act-proxy（AppPolicyScreen.tsx）
    '#s-nodes.nodes-list-view .nd-card:hover', // .nd-top .nd-lat.fast/.slow
  ] as const;

  /**
   * 登记宿主的 `--surface-2/α`，取最暗那一档。宿主改名/改写法 ⇒ 抛错，不静默缩表。
   * 同一个宿主在多个文件里各有一份（prototype/screens 常成对），**取各份里最暗的**而不是第一份 ——
   * 谁最后 @import 谁生效这件事不该由本门去猜，取最暗才是保守的那一侧。
   */
  const rowHoverAlpha = () => {
    const alphas = TINT_HOSTS.map((host) => {
      const found = STYLE_FILES.flatMap((f) => flat(stripComments(read(f))))
        .filter((r) => r.sel === host)
        .map((r) => /background\s*:\s*hsl\(var\(--surface-2\)\s*\/\s*([\d.]+)\s*\)/.exec(r.body))
        .filter((m): m is RegExpExecArray => m !== null)
        .map((m) => +m[1]);
      if (!found.length)
        throw new Error(`登记的 chip 宿主 \`${host}\` 已经不是 --surface-2/α 的 hover 底了`);
      return Math.max(...found);
    });
    return Math.max(...alphas);
  };

  /** 「该色**当文字**时自带的淡染底」：同一条规则里 color 与 background 都指向该 token。 */
  const selfTintAlphas = (tok: Tok) => {
    const colorRe = new RegExp(`color\\s*:\\s*hsl\\(var\\(${tok}\\)\\s*\\)`);
    const bgRe = new RegExp(`background\\s*:\\s*hsl\\(var\\(${tok}\\)\\s*/\\s*([\\d.]+)\\s*\\)`);
    const out = new Set<number>();
    for (const f of STYLE_FILES)
      for (const r of flat(stripComments(read(f)))) {
        const a = bgRe.exec(r.body);
        if (a && colorRe.test(r.body)) out.add(+a[1]);
      }
    return [...out].sort((a, b) => a - b);
  };

  /**
   * 花名册。`color` 传进来而不是内部取，是为了 ⑥ 能拿改前的值跑同一张表。
   */
  const backdrops = (tok: Tok, band: 'light' | 'dark', color: RGB) => {
    const block = protoBand(band);
    const T = (n: string) => hslToRgb(must(readVar(block.body, n), `${band} 令牌块里没有 ${n}`));
    const surface = T('--surface');
    const out: { what: string; rgb: RGB }[] = [
      { what: '--surface（卡片/弹窗/菜单里的该色文字）', rgb: surface },
      { what: '--surface-2（状态栏 / 托盘行 hover / 节点选单 hover）', rgb: T('--surface-2') },
      { what: '--bg（卡片外的屏幕底）', rgb: T('--bg') },
    ];
    const weak = TOKENS[tok].weak;
    if (weak) out.push({ what: `${weak}（.pill / .nd-cap.lan / .ut-ic 一族）`, rgb: T(weak) });

    const alpha = rowHoverAlpha();
    const rowHover = over(T('--surface-2'), alpha, surface);

    const tints = selfTintAlphas(tok);
    if (tok !== '--dn' && !tints.length)
      throw new Error(`${tok} 一处自淡染 chip 都没扫到 —— 扫描逻辑失效了`);
    for (const a of tints) {
      out.push({ what: `${tok}/${a} 叠在 --surface 上`, rgb: over(color, a, surface) });
      out.push({
        what: `${tok}/${a} 叠在行 hover 底（--surface-2/${alpha}）上`,
        rgb: over(color, a, rowHover),
      });
    }
    return out;
  };

  const failures = (tok: Tok, band: 'light' | 'dark', color: RGB) =>
    backdrops(tok, band, color)
      .map((b) => ({ ...b, ratio: contrast(color, b.rgb) }))
      .filter((b) => b.ratio < 4.5)
      .map((b) => `${band} ${tok} 落 ${b.what} = ${b.ratio}`);

  it('⓪ 自检：本文件的对比度算术能复现改前实测值（否则下面的数全是自说自话）', () => {
    const s2 = hslToRgb('210 28% 94%');
    expect(contrast(hslToRgb('32 84% 42%'), s2), '旧 warn 落 --surface-2：实测 3.19').toBe(3.19);
    expect(contrast(hslToRgb('152 60% 36%'), s2), '旧 ok 落 --surface-2：实测 3.38').toBe(3.38);
    expect(contrast(hslToRgb('197 80% 42%'), s2), '旧 dn 落 --surface-2：实测 3.16').toBe(3.16);
    expect(contrast(hslToRgb('0 0% 100%'), hslToRgb('356 74% 66%')), '深色白字落 err 实底：实测 3.16').toBe(3.16);
    // L* 走上面那个 `lum()`（WCAG 的三位系数），比 D65 全精度系数低 0.01 —— 注释里报的就是这一套。
    expect(lstar(hslToRgb('356 68% 44%')), '浅色 --err 的 CIE L*：41.45').toBe(41.45);
    expect(lstar(hslToRgb('32 84% 31%')), '新 warn 的 CIE L*：41.59').toBe(41.59);
    expect(lstar(hslToRgb('152 60% 27%')), '新 ok 的 CIE L*：41.00').toBe(41);
    expect(lstar(hslToRgb('197 80% 33%')), '新 dn 的 CIE L*：44.47').toBe(44.47);
  });

  it('① 三个 token 与 --err 同在一条覆盖块里，且落在最后一个 @import 之后', () => {
    const rule = overrideRule();
    for (const t of [...TOK_NAMES, '--err' as const])
      expect(readVar(rule.body, t), `覆盖块里没有 ${t} —— tokens.css 那份是死代码，写那儿等于没写`).toBeTruthy();
    const lastImport = indexCss.lastIndexOf('@import');
    expect(lastImport, 'index.css 里一个 @import 都没有 —— 走查逻辑失效了').toBeGreaterThan(0);
    expect(
      indexCss.indexOf(rule.body),
      'prototype.css 是最后一个 @import 且自带同选择器令牌块 ⇒ 覆盖写在它前面 = 空操作'
    ).toBeGreaterThan(lastImport);
  });

  it('② 覆盖只挂浅色选择器（挂上深色 = 把已经达标的深色一起改坏）', () => {
    const parts = overrideRule()
      .sel.split(',')
      .map((s) => s.trim())
      .sort();
    expect(parts, '浅色令牌覆盖的选择器变了').toEqual([':root', ":root[data-theme='light']"]);
  });

  it('③ tokens.css 的两条浅色腿与覆盖同值（真值源不许说谎）', () => {
    const legs = flat(tokensCss).filter(
      (r) => (r.sel === ':root' || r.sel === ":root[data-theme='light']") && readVar(r.body, '--bg')
    );
    expect(legs.length, 'tokens.css 的两个浅色档没找齐').toBe(2);
    for (const t of TOK_NAMES) {
      const value = readVar(overrideRule().body, t);
      for (const leg of legs)
        expect(
          readVar(leg.body, t),
          `tokens.css ${leg.sel} 的 ${t} 与 index.css 的生效覆盖不同值 —— 那份进不了浏览器，留着旧值只会骗下一个人`
        ).toBe(value);
    }
  });

  it('④ 浅色：三个 token 当文字落在每一种实际底色上都 ≥4.5:1', () => {
    const bad = TOK_NAMES.flatMap((t) => failures(t, 'light', effective(t, 'light')));
    expect(
      bad,
      '浅色语义色文字有底色不过 WCAG AA 4.5:1。改 token 明度（根因），别给单处另开 token，也别放宽到 3:1。'
    ).toEqual([]);
  });

  it('⑤ 深色：同三个 token 也 ≥4.5:1（「深色不动」是量出来的结论，不是没测）', () => {
    const bad = TOK_NAMES.flatMap((t) => failures(t, 'dark', effective(t, 'dark')));
    expect(bad, '深色语义色文字不过 AA —— 深色档也得改，不能只修浅色').toEqual([]);
  });

  it('⑥ 自检：花名册有牙 —— 拿改前的三个旧值跑同一张表必须大面积翻红', () => {
    let n = 0;
    for (const t of TOK_NAMES) {
      const old = hslToRgb(TOKENS[t].before);
      const list = backdrops(t, 'light', old);
      expect(list.length, `${t} 的底色花名册太短（${list.length}）—— ④ 在近似空集上恒绿`).toBeGreaterThanOrEqual(3);
      const f = failures(t, 'light', old).length;
      expect(f, `用改前的 ${t} 跑，这张表居然一条都不红 —— 花名册漏了本次修的那些底色`).toBeGreaterThan(0);
      n += f;
    }
    expect(n, '三个 token 用旧值合计翻红条数太少 —— 花名册在缩水').toBeGreaterThanOrEqual(10);
  });

  it('⑦ 深色 .btn.danger 换 --surface 墨且避开禁用态；浅色仍是白字且已达标', () => {
    const legs = flat(indexCss).filter((r) => /\.btn\.danger:not\(:disabled\)/.test(r.sel));
    expect(
      legs.map((r) => r.sel),
      'index.css 里 `.btn.danger:not(:disabled)` 的墨色覆盖不是恰好两条（media 腿 + 显式腿）'
    ).toHaveLength(2);
    expect(
      legs.filter((r) => /:root:not\(\[data-theme=['"]light['"]\]\)/.test(r.sel)).length,
      '缺 @media(prefers-color-scheme:dark) 那条腿 ⇒ 跟随系统深色的用户拿不到'
    ).toBe(1);
    expect(
      legs.filter((r) => /:root\[data-theme=['"]dark['"]\]/.test(r.sel)).length,
      '缺显式 [data-theme="dark"] 那条腿 ⇒ 手动切深色的用户拿不到'
    ).toBe(1);
    // `:not(:disabled)` 不是装饰：少了它本条 (0,4,0) 会与 `:root .btn.danger:disabled` 同特异性、
    // 且源序在后 ⇒ 顶掉禁用态的 --fg-faint，换成近黑墨压 --surface-2 禁用底（深色下 ~1.1，字消失）。
    for (const r of legs) {
      expect(r.sel, `这条腿丢了 :not(:disabled)（${r.sel}）—— 会把禁用态的墨顶成近黑`).toContain(
        ':not(:disabled)'
      );
      const ink = must(/color\s*:\s*([^;]+)/.exec(r.body), '覆盖规则里没有 color 声明')[1].trim();
      expect(ink, `墨色不是 hsl(var(--surface))（${ink}）—— 裸色值只在一档成立`).toBe('hsl(var(--surface))');
    }
    const darkErr = effective('--err', 'dark');
    const darkSurface = hslToRgb(must(readVar(protoBand('dark').body, '--surface'), '深色块没有 --surface'));
    expect(
      contrast(darkSurface, darkErr),
      `深色 .btn.danger 墨色压 err 实底仍不过 AA（${contrast(darkSurface, darkErr)}）`
    ).toBeGreaterThanOrEqual(4.5);
    // 前提校验 + 浅色档：基础规则仍是白字，而浅色 --err 已下调到白字够看的档。
    expect(stripComments(read('./components.css')), '.btn.danger 的基础规则变了 —— 本门的前提没了').toMatch(
      /\.btn\.danger\s*\{[^}]*color\s*:\s*#fff/
    );
    const lightWhite = contrast(hslToRgb('0 0% 100%'), effective('--err', 'light'));
    expect(lightWhite, `浅色 .btn.danger 白字压 err 实底不过 AA（${lightWhite}）`).toBeGreaterThanOrEqual(4.5);
  });

  it('⑧ 随 --ok/--warn 下调而必须改的两处墨色都在，且两档都过 AA', () => {
    const stepInk = flat(indexCss).filter((r) => r.sel === '.hc-step.done .st-n');
    expect(stepInk, 'index.css 缺 `.hc-step.done .st-n` 的墨色覆盖 ⇒ 10.5px 序号掉到 2.99').toHaveLength(1);
    expect(stepInk[0].body).toMatch(/color\s*:\s*hsl\(var\(--surface\)\)/);
    const markInk = flat(indexCss).filter((r) => r.sel === '.log-view mark');
    expect(markInk, 'index.css 缺 `.log-view mark` 的墨色覆盖 ⇒ 命中片段沿用行色，压在 warn/0.5 上不可读').toHaveLength(1);
    expect(markInk[0].body).toMatch(/color\s*:\s*hsl\(var\(--fg\)\)/);

    // `.log-view` 的底与 `mark` 的淡染 α 都从 prototype.css 现读 —— 那两个数一改，本条跟着变。
    const logAlpha = +must(
      /\.log-view\s*\{[^}]*background\s*:\s*hsl\(var\(--bg\)\s*\/\s*([\d.]+)\s*\)/.exec(protoCss),
      '.log-view 的半透明底没扫到'
    )[1];
    const markAlpha = +must(
      /\.log-view\s+mark\s*\{[^}]*background\s*:\s*hsl\(var\(--warn\)\s*\/\s*([\d.]+)\s*\)/.exec(protoCss),
      '.log-view mark 的淡染底没扫到'
    )[1];
    for (const band of ['light', 'dark'] as const) {
      const B = protoBand(band);
      const T = (n: string) => hslToRgb(must(readVar(B.body, n), `${band} 令牌块里没有 ${n}`));
      const ok = band === 'light' ? effective('--ok', 'light') : T('--ok');
      const warn = band === 'light' ? effective('--warn', 'light') : T('--warn');
      const step = contrast(T('--surface'), ok);
      expect(step, `${band} 档步骤序号（--surface 墨压 --ok 实底）= ${step}`).toBeGreaterThanOrEqual(4.5);
      const mark = contrast(T('--fg'), over(warn, markAlpha, over(T('--bg'), logAlpha, T('--surface'))));
      expect(mark, `${band} 档日志高亮（--fg 墨压 warn/${markAlpha} 高亮底）= ${mark}`).toBeGreaterThanOrEqual(4.5);
    }
  });
});

describe('主窗最小尺寸契约', () => {
  it('默认尺寸不得小于 980×740，且与最小尺寸一致', () => {
    const conf = JSON.parse(read('../../../src-tauri/tauri.conf.json'));
    const main = conf.app.windows.find((window: { label?: string }) => window.label === 'main');
    expect(main).toMatchObject({
      width: 980,
      height: 740,
      minWidth: 980,
      minHeight: 740,
    });
  });
});

describe('规则资源表：列边界不得随「这一行有几颗按钮」漂移（2026-08-05 真机：大小列内置/外置错位）', () => {
  /** 每行虽是独立 grid，但两份规范样式必须共享唯一轨道；index.css 不再承担事后修补。 */
  const protoCss = stripComments(read('./prototype.css'));
  const screensCss = stripComments(read('./screens.css'));
  const indexCss = stripComments(read('./index.css'));
  const resourceScreen = read('../components/screens/resources/ResourcesScreen.tsx');

  /** 从 `@container mainc (max-width:N)` 块里取 `.res-row` 的轨道定义。 */
  const narrowOf = (css: string) => {
    const m = css.match(
      /@container\s+mainc\s*\(\s*max-width:\s*(\d+)px\s*\)\s*\{[^{}]*\.res-row\s*\{[^{}]*grid-template-columns:\s*([^;}]+)/
    );
    if (!m) throw new Error('解析不到 .res-row 的窄档轨道定义');
    return { bp: Number(m[1]), tracks: m[2].trim() };
  };
  const wideTracks = (css: string) => {
    const m = css.match(/\.res-row\s*\{\s*display:\s*grid;\s*grid-template-columns:\s*([^;}]+)/);
    if (!m) throw new Error('解析不到 .res-row 的宽档轨道定义');
    return m[1].trim();
  };
  it('两份规范样式的宽档固定为 1fr / 80 / 132 / 60，窄档固定为 1fr / 80 / 60', () => {
    for (const [name, css] of [['prototype.css', protoCss], ['screens.css', screensCss]] as const) {
      expect(wideTracks(css), `${name} 宽档`).toBe('minmax(0,1fr) 80px 132px 60px');
      expect(narrowOf(css), `${name} 窄档`).toEqual({
        bp: 780,
        tracks: 'minmax(0,1fr) 80px 60px',
      });
      expect(css).toMatch(/\.res-row\s*\{[^}]*column-gap:\s*12px/);
      expect(css).toMatch(/\.res-actions\s*\{[^}]*justify-content:\s*flex-end/);
    }
  });

  it('60px 动作轨恰好容纳两颗图标按钮，并且组件不再以内联 margin 修补间距', () => {
    const btn = protoCss.match(/\.nd-a\s*\{[^}]*width:\s*(\d+)px/);
    const gap = protoCss.match(/\.res-row\s*>\s*span:last-child\s*\{[^}]*gap:\s*(\d+)px/);
    expect(btn, '解析不到 .nd-a 宽度').not.toBeNull();
    expect(gap, '解析不到动作格 gap').not.toBeNull();
    const widest = Number(btn![1]) * 2 + Number(gap![1]);
    expect(widest).toBe(60);
    expect(resourceScreen.match(/className="res-actions"/g)?.length ?? 0).toBeGreaterThanOrEqual(5);
    expect(resourceScreen).not.toContain("style={{ textAlign: 'right' }}");
    expect(resourceScreen).not.toContain('style={{ marginLeft: 6 }}');
  });

  it('index.css 不得重新覆盖资源表轨道', () => {
    expect(indexCss).not.toMatch(/\.res-row\s*\{[^}]*grid-template-columns/);
  });

  it('980px 最小窗口使用宽档，780px 窄档只作更窄窗口的防御性兜底', () => {
    const conf = JSON.parse(read('../../../src-tauri/tauri.conf.json'));
    const minW = conf.app.windows[0].minWidth;
    const side = protoCss.match(/\.side\s*\{\s*width:\s*(\d+)px/);
    expect(side, '解析不到 .side 宽度').not.toBeNull();
    expect(typeof minW, 'tauri.conf.json 里读不到 minWidth').toBe('number');
    expect(
      minW - Number(side![1]),
      `.main 的最小内联宽 ${minW}-${side![1]} 仍命中窄档 —— 980px 窗口没有切回四列宽档`
    ).toBeGreaterThan(narrowOf(protoCss).bp);
  });
});
