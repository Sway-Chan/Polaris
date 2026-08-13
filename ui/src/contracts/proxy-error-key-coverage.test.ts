/**
 * 代理错误码 → i18n 键的**跨语言覆盖门**：Rust `runtime/proxy.rs::mod code` ↔ 前端映射表 ↔ 五份 locale。
 *
 * # 这道门守的是什么
 *
 * 缺陷原形：渲染端六处写成 `data.message || t('home.proxyCrashed', '代理已断开')`，而后端
 * `emit_proxy_error(message, error_code)` 两参皆非可选 ⇒ `||` 右边**永远短路不到**。那些 i18n key
 * 名义上存在、实际是死键，俄语/波斯语用户在最高频的错误路径上看到 Rust 侧写死的中文。
 * 这是一个**全绿的缺陷**：`tsc` 过、`vitest` 过、`locale-parity` 过（键确实五语齐备，只是没人渲染），
 * 只有非中文用户会撞上。修法是让前端按 `errorCode` 取键（`domain/proxy-error-text.ts`），
 * 而修完之后立刻出现两条新的、**同样静默**的漂移面：
 *
 *  1. **Rust 新增一个 `error_code`、前端映射表没跟** ⇒ 该码落到三段式第 2 段，用户又看到中文串。
 *     TypeScript 抓不到（`errorCode` 运行期就是个 string），任何纯前端测试也抓不到
 *     （前端表自己跟自己比永远一致）。
 *  2. **映射表指向的键在某个语种里不存在** ⇒ i18next 回落到键名，用户看到 `home.proxyCrashed`
 *     这串开发者标识符。`locale-parity` 的 ru/fa 是**棘轮**（允许 451 个缺口），故它**不会**
 *     替这几个键说话 —— 它们必须被单独钉死为「零缺口」。
 *
 * 两条都只有把 Rust 源码与五份 locale 同时读进来才判得了，这就是本文件。
 *
 * # 为什么读 Rust 源码而不是抄一份镜像常量
 *
 * 抄镜像只是把漂移面往后挪一格（改了源不改镜像 = 门在守一个假真值）。范式照抄同目录的
 * `rust-i18n-coverage.test.ts` / `protocol-settings-coverage.test.ts` 与
 * `components/layout/nonfatal-error-codes.test.ts`：把 Rust 源码当真值读进来解析。
 *
 * # 五条判据
 *
 *  G1 **码集双向精确相等**：Rust `mod code` 的常量集 = 映射表键集 ∪ 豁免表键集。
 *     正向抓「新增码没配键也没豁免」，反向抓「码删了/改名了，前端表里留着死项」。
 *  G2 **豁免表纪律**：与映射表不得相交；每条理由非空且足够具体（不许写 `''` 或 `'TODO'` 蒙混）。
 *  G3 **键在五语种都有真译文**：映射表的每个 value 在五份 locale 里都必须是非空字符串。
 *  G4 **接线**：`App.tsx` 里被 `errorCode === '…'` 路由的码，必须全在「映射表 ∪ 豁免表」里；
 *     且七处 toast 文案确实走 `proxyErrorText(`，不是又在组件里手写一套。
 *  G5 **缺陷形态零容忍**：全仓非测试源码里不得再出现 `X.message || t(...)`（含 `i18n.t`）。
 *     这条是缺陷本身的哨兵 —— 上面四条都盯着「表配没配全」，只有它盯着「有没有人把优先级换回去」。
 *
 * 另有一组三段式的**行为**断言（不读源码，直接调 `proxyErrorReason` / `proxyErrorText`）：
 * 三段各一条，尤其第 2 段（`errorCode` 缺失 / 未配键时必须回落 `message`，而不是跳到通用兜底）。
 *
 * # 读不到就抛，不跳过
 *
 * 所有解析（`mod code` 段、App.tsx 分腿、locale 文件、源码清单）解析不到一律 `throw`。
 * 「扫到 0 条于是 0 个断言全绿」是假门 —— 那样 Rust 模块一改名，门就静默消失，
 * 「没检查」与「检查通过」的输出不可区分。
 *
 * # 抓不到什么（射程自曝）
 *
 *  a. **译文对不对**：只管「有没有、空不空」，不管译得准不准。
 *  b. **Rust 是否真的会发某个码**：判据是 `mod code` 里**声明**了哪些常量，不是哪些有活的
 *     `set_error`/`set_nonfatal_error` 调用点。声明了却没人发的码会被要求配键（多配一个键无害）。
 *  c. **码被路由到哪条腿**（toast 等级 / 发不发桌面通知）：那是 `App.test.ts` 的射程。
 *     本门只管「这个码有没有一句能给用户看的、本地化的话」。
 *  d. **`ApiResponse::err_with_code` 那一套 `code`**（`PRIVACY_LOCKED` / `WARP_*` / `TAILSCALE_*` …）：
 *     它们走的是 IPC 信封的 `code` 字段 → `IpcError`，由各调用点的 `catch` 自行处理，
 *     与 `event:proxyError` 的 `errorCode` 不是同一个取值集，不在本门射程内。
 */

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

import { SUPPORTED_LANGUAGES } from '../domain/language';
import {
  PROXY_ERROR_TEXT_KEY,
  proxyErrorReason,
  proxyErrorText,
} from '../domain/proxy-error-text';

/**
 * **判断过后决定不配 i18n 键**的码 → 理由。G1 要求「映射表 ∪ 本表 = Rust 码集」，
 * 故每个 Rust 码要么有键、要么在这里说明白为什么没有 —— 两者都没有 ⇒ 红。
 *
 * 豁免住在门里而不是 `domain/proxy-error-text.ts`：它是纯门元数据，运行期一次都不读
 * （同 `protocol-settings-coverage.test.ts` 的 `EXEMPT`）。
 *
 * 共同判据：这两条的后端 `message` 携带**本次失败独有的可诊断信息**，而任何 i18n 键只能给出
 * 同义反复。给它们配键 = 用「语种正确」换掉「唯一能指出下一步动作的那句话」。它们照常经
 * 三段式**第 2 段**把后端那句准确的话交给用户 —— 豁免的是「配键」，不是「出声」。
 */
const KEY_EXEMPT: Readonly<Record<string, string>> = {
  STARTUP_FAILED:
    'message 是这一次起核失败的具体原因（就绪门超时 / 核启动期退出 + 核 stderr 摘要），' +
    '键只能给同义反复的「启动失败，请检查服务器配置」，等于把唯一可诊断的那句话换掉。' +
    '另：handleProxyErrorEvent 刻意不路由本码（发起方 await 腿自己 toast errors.startupFailed；' +
    '两处都报 = 同一次失败弹两遍，App.test.ts 有「STARTUP_FAILED 零弹」断言守着），' +
    '它只经 pending bar 的 lifecycle 腿出声。',
  ROOT_ORPHAN_BLOCKED:
    'message 带残留 root 孤儿的 pid —— 用户下一步（sudo kill 掉那个 pid，或修 helper）完全依赖它，键给不出。' +
    'handleProxyErrorEvent 已路由本码（同 HELPER 两码：刷连接态 + 认领闸门去重 + toast.error 走第 2 段 + ' +
    '桌面通知，见 App.tsx 头注对应小节），HomeScreen 的 await 腿也已有专属 errors.rootOrphanBlocked ' +
    '标题（不再落「检查服务器配置」的 errors.startupFailed 兜底）。豁免的只是「配 i18n 键」这一项，' +
    '不是「不路由」——message 的 pid 仍恒经三段式第 2 段原文送达用户。',
  CORE_BINARY_MISMATCH:
    'message 带三样只有后端知道的事实：**实际在跑的那个二进制的绝对路径**、它自报的版本、' +
    '以及本次期望的版本，末尾还给出可行动指引（重装提权助手 / 重新执行内核更新）。' +
    '这三样正是这条自证的全部价值——它存在的理由就是「换核静默失效」，而键只能给出' +
    '「内核版本不一致」这句同义反复，把用户唯一能据以判断「旧核旧在哪、该怎么办」的信息删掉。' +
    '本码是**非终态**（核已起来并在服务流量，只是跑的是旧核），故同时登记在 ' +
    'pending-bar-logic 的 CORE_STILL_RUNNING_CODES 里，不让条卡死在「应用中…」。',
};

const SRC_DIR = fileURLToPath(new URL('..', import.meta.url));
const LOCALES_DIR = fileURLToPath(new URL('../i18n/locales', import.meta.url));

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

const RUST_PROXY = read('../../../src-tauri/src/runtime/proxy.rs');
const APP_TSX = read('../App.tsx');

// ════════════════════════════════════════════════════════════════════════════
// 解析器（任何一处解析不到都抛）
// ════════════════════════════════════════════════════════════════════════════

/**
 * Rust `pub mod code { … }` 里声明的全部码字面量。
 *
 * 大括号配平取模块体（同 `nonfatal-error-codes.test.ts` 的做法）：按行数或正则截断都会在
 * 模块里出现嵌套块时静默取错段。
 */
function rustErrorCodes(): string[] {
  const start = RUST_PROXY.indexOf('pub mod code {');
  if (start < 0) {
    throw new Error(
      'proxy.rs 里找不到 `pub mod code {` —— 模块被改名/搬走（那么本门该跟着改），或解析器坏了。两种情况都不许静默放行。'
    );
  }
  const bodyStart = RUST_PROXY.indexOf('{', start);
  let depth = 0;
  let end = -1;
  for (let i = bodyStart; i < RUST_PROXY.length; i += 1) {
    if (RUST_PROXY[i] === '{') depth += 1;
    else if (RUST_PROXY[i] === '}') {
      depth -= 1;
      if (depth === 0) {
        end = i;
        break;
      }
    }
  }
  if (end < 0) throw new Error('`mod code` 大括号不配平 —— 解析器失效');
  const codes = [
    ...RUST_PROXY.slice(bodyStart, end).matchAll(/pub const [A-Z_0-9]+: &str = "([A-Z_0-9]+)";/g),
  ].map((m) => m[1]);
  if (codes.length === 0) throw new Error('`mod code` 里一个常量都没解析到 —— 写法变了？');
  return codes;
}

/** `App.tsx` 里被 `data.errorCode === 'X'` 路由的码。 */
function routedCodesInApp(): string[] {
  const codes = [...APP_TSX.matchAll(/data\.errorCode === '([A-Z_0-9]+)'/g)].map((m) => m[1]);
  if (codes.length === 0) {
    throw new Error(
      'App.tsx 里一条 `data.errorCode === \'…\'` 分腿都没解析到 —— 分腿写法变了，本门已失去判据'
    );
  }
  return codes;
}

const flatten = (o: unknown, p = '', out: Record<string, string> = {}) => {
  for (const [k, v] of Object.entries(o as Record<string, unknown>)) {
    const kk = p ? `${p}.${k}` : k;
    if (typeof v === 'string') out[kk] = v;
    else if (v !== null && typeof v === 'object') flatten(v, kk, out);
  }
  return out;
};

const LOCALES: Record<string, Record<string, string>> = Object.fromEntries(
  SUPPORTED_LANGUAGES.map((l) => [
    l,
    flatten(JSON.parse(readFileSync(join(LOCALES_DIR, `${l}.json`), 'utf8'))),
  ])
);

/** `ui/src/**` 的非测试 `.ts` / `.tsx`（G5 用）。 */
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
  if (acc.length < 100) throw new Error(`只收集到 ${acc.length} 个源码文件 —— 收集器失效`);
  return acc;
}

// ════════════════════════════════════════════════════════════════════════════
// G0 自检：本门不能空转
// ════════════════════════════════════════════════════════════════════════════

describe('G0 自检：语料与解析都非空', () => {
  it('Rust 码集 / 映射表 / locale 三者都解析到了东西', () => {
    expect(rustErrorCodes().length, 'Rust 码一个都没解析到').toBeGreaterThanOrEqual(8);
    expect(Object.keys(PROXY_ERROR_TEXT_KEY).length, '映射表为空').toBeGreaterThanOrEqual(6);
    expect(Object.keys(LOCALES).length, 'locale 语种不齐').toBe(5);
    for (const l of SUPPORTED_LANGUAGES) {
      expect(Object.keys(LOCALES[l]).length, `${l} 的语料为空`).toBeGreaterThan(100);
    }
    expect(routedCodesInApp().length, 'App.tsx 分腿一条都没解析到').toBeGreaterThanOrEqual(6);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G1 码集双向精确相等
// ════════════════════════════════════════════════════════════════════════════

describe('G1 Rust `error_code` 取值集 = 前端映射表 ∪ 豁免表', () => {
  it('双向精确相等（正向抓漏配，反向抓死项）', () => {
    const rust = [...rustErrorCodes()].sort();
    const covered = [
      ...Object.keys(PROXY_ERROR_TEXT_KEY),
      ...Object.keys(KEY_EXEMPT),
    ].sort();
    // 变异实测（真跑过）：在 `mod code` 里加一条 `pub const FOO_BAR: &str = "FOO_BAR";`
    // 而不动前端两张表 ⇒ 本条转红，报「Rust 有 FOO_BAR 而前端两张表都没有」。
    expect(
      covered,
      'Rust 的 error_code 取值集与前端「映射表 ∪ 豁免表」分叉 —— ' +
        '多出来的码会落到三段式第 2 段（用户看到 Rust 的中文串），少掉的是改名后残留的死项'
    ).toEqual(rust);
  });

  it('映射表与豁免表不得相交（一个码要么有键、要么明确说明为什么没有）', () => {
    const both = Object.keys(PROXY_ERROR_TEXT_KEY).filter((c) =>
      Object.prototype.hasOwnProperty.call(KEY_EXEMPT, c)
    );
    expect(both, '这些码同时出现在映射表与豁免表里 —— 豁免理由已失效，删掉豁免').toEqual([]);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G2 豁免表纪律
// ════════════════════════════════════════════════════════════════════════════

describe('G2 豁免必须带得住的理由', () => {
  it('每条豁免的理由非空且足够具体', () => {
    // 豁免表是这道门唯一的口子。理由写不出来 = 这个码其实是漏配的，不是判断过的。
    // 40 字的下限不是形式主义：它刚好挡住 'TODO' / '暂不需要' / '内部错误' 这类没有因果的搪塞。
    const thin = Object.entries(KEY_EXEMPT)
      .filter(([, reason]) => reason.trim().length < 40)
      .map(([code]) => code);
    expect(thin, '豁免理由太短 —— 说不出「为什么这个码不该有 i18n 键」就不该豁免').toEqual([]);
  });

  it('豁免的码必须是 Rust 真实存在的码（改名后的死豁免 → 红）', () => {
    const rust = new Set(rustErrorCodes());
    const dead = Object.keys(KEY_EXEMPT).filter((c) => !rust.has(c));
    expect(dead, '豁免表里挂着 Rust 已不存在的码 —— 死豁免会变成永久盲区').toEqual([]);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G3 键在五语种齐备
// ════════════════════════════════════════════════════════════════════════════

describe('G3 映射表指向的键在五份 locale 里都有真译文', () => {
  it('零缺口（这几个键不受 locale-parity 的 ru/fa 棘轮庇护）', () => {
    // `locale-parity` 允许 ru/fa 有 451 个缺口，故它**不会**替这几个键说话。缺一条的症状：
    // i18next 回落键名，用户看到 `home.proxyCrashed` 这串开发者标识符。
    // 变异实测（真跑过）：删掉 `ru.json` 里的 `home.proxyCrashed` ⇒ 本条转红。
    const missing: string[] = [];
    for (const [code, key] of Object.entries(PROXY_ERROR_TEXT_KEY)) {
      for (const l of SUPPORTED_LANGUAGES) {
        const v = LOCALES[l][key];
        if (typeof v !== 'string' || v.trim() === '') missing.push(`${l}: ${key} (← ${code})`);
      }
    }
    expect(missing.sort(), '错误码用到的 i18n 键没有五语种齐备').toEqual([]);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G4 接线：判据面与渲染面都得真接上
// ════════════════════════════════════════════════════════════════════════════

describe('G4 接线：App.tsx 的分腿与映射表对得上，且文案真的走解析器', () => {
  it('每个被路由的码都在「映射表 ∪ 豁免表」里（漏一个 ⇒ 该腿回落中文 message 且未经判断）', () => {
    const unmapped = [...new Set(routedCodesInApp())].filter(
      (c) =>
        !Object.prototype.hasOwnProperty.call(PROXY_ERROR_TEXT_KEY, c) &&
        !Object.prototype.hasOwnProperty.call(KEY_EXEMPT, c)
    );
    // 变异实测（真跑过）：给 App.tsx 加一条 `if (data.errorCode === 'FOO_BAR')` 分腿，FOO_BAR 既不进
    // 映射表也不登记豁免 ⇒ 本条转红。ROOT_ORPHAN_BLOCKED 是本门唯一「路由但豁免配键」的例外——
    // 理由见上面 KEY_EXEMPT 的登记：message 带残留进程的 pid，不许被 i18n 键的同义反复换掉，
    // 但这不代表「没人判断过」，故不该被本条拦下。
    expect(
      unmapped,
      'App.tsx 路由了这些码，但它们既没有 i18n 键也没有登记豁免理由 —— 该腿会回落成未经判断的中文 message'
    ).toEqual([]);
  });

  it('六处 toast 文案都走 `proxyErrorText(`，不在组件里另写一套', () => {
    const stripped = APP_TSX.replace(/^\s*(\/\/|\*|\/\*).*$/gm, ''); // 注释里逐字写着反例
    const calls = [...stripped.matchAll(/proxyErrorText\(data, t\)/g)].length;
    expect(calls, 'handleProxyErrorEvent 的 toast 文案没有全部走解析器').toBeGreaterThanOrEqual(6);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// G5 缺陷形态零容忍
// ════════════════════════════════════════════════════════════════════════════

describe('G5 全仓不得再出现 `X.message || t(...)`', () => {
  it('零处（这是缺陷本身的形状，不是风格问题）', () => {
    // 上面四条盯的都是「表配没配全」；只有这条盯着「有没有人把优先级换回去」。
    // 变异实测（真跑过）：把 App.tsx 任一处改回 `data.message || t('home.proxyCrashed')` ⇒ 转红。
    const shape = /\.message\s*\|\|\s*(?:i18n\.)?t\(/;
    const bad: string[] = [];
    for (const f of collectSources()) {
      readFileSync(f, 'utf8')
        .split('\n')
        // 剔整行注释（同 `nonfatal-error-codes.test.ts` / `pending-bar-logic.test.ts` 的口径）：
        // 本门与被修的两个文件的头注里都**逐字引用**了这个反例，不剔就会红在自己的说明文字上
        // ——那是真阳性的判定逻辑打在假阳性的目标上。代价：行尾注释里写这个形状抓不到，无实害。
        .forEach((line, i) => {
          if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
          if (shape.test(line)) bad.push(`${f.slice(SRC_DIR.length)}:${i + 1}`);
        });
    }
    expect(
      bad.sort(),
      '后端 message 又压过了 i18n key —— 后端 message 是 Rust 侧写死的中文串，ru/fa 用户会看到中文'
    ).toEqual([]);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// 三段式行为断言（每一段各自可失效，故各有一条）
// ════════════════════════════════════════════════════════════════════════════

describe('三段式判定：key → message → 通用兜底', () => {
  const t = (key: string) => `T:${key}`;

  it('① errorCode 有、且映射表有键 → 用键（即使后端同时给了 message）', () => {
    for (const [code, key] of Object.entries(PROXY_ERROR_TEXT_KEY)) {
      const got = proxyErrorReason({ errorCode: code, message: '后端的中文串' }, t);
      expect(got, `${code} 没走第 1 段`).toBe(`T:${key}`);
    }
  });

  it('② errorCode 缺失 → 回落 message（**不许**跳到通用兜底）', () => {
    // 这一段是整条规则里唯一「用准确性换语种」会出事的地方：`errorCode` 在
    // `ProxyLifecycleEvent` 上是真可选（后端「无可诚实断言的分类」那类失败腿不带码）。
    // 省掉它 = 把「后端诚实地说不出分类」变成「用户看到一句无信息量的通用错误」。
    // 变异对照：把第 2 段删掉 ⇒ 本条与下一条同时转红。
    expect(proxyErrorReason({ message: 'sing-box 启动期退出: bad config' }, t)).toBe(
      'sing-box 启动期退出: bad config'
    );
    expect(proxyErrorText({ message: 'sing-box 启动期退出: bad config' }, t)).toBe(
      'sing-box 启动期退出: bad config'
    );
  });

  it('② 豁免码（有码但故意没键）同样回落 message', () => {
    for (const code of Object.keys(KEY_EXEMPT)) {
      expect(proxyErrorReason({ errorCode: code, message: '具体原因' }, t)).toBe('具体原因');
    }
  });

  it('② 未知码（版本漂移 / 脏值）也回落 message，不渲染裸键名', () => {
    // 这正是「显式映射表」相对「约定拼接 `errors.<code>`」的价值：拼接法在这里会把
    // `errors.someNewCode` 这串标识符渲染给用户，映射表则把后端那句准确的话交出去。
    expect(proxyErrorReason({ errorCode: 'SOME_NEW_CODE', message: '后端说的话' }, t)).toBe(
      '后端说的话'
    );
  });

  it('③ 两者皆无 → reason 归 null / text 落通用兜底句', () => {
    expect(proxyErrorReason({}, t)).toBeNull();
    expect(proxyErrorReason({ errorCode: 'SOME_NEW_CODE' }, t)).toBeNull();
    // 空串 / 全空白与缺字段同等对待（否则 toast 会渲染出一条空白提示）。
    expect(proxyErrorReason({ message: '   ' }, t)).toBeNull();
    expect(proxyErrorText({}, t)).toBe('T:errors.operationFailed');
  });

  it('原型链上的属性不得被当成键（errorCode 是从 IPC 收来的任意串）', () => {
    // 直接下标会让 `'toString'` / `'constructor'` 取到 Object.prototype 上的函数，
    // 落进 `t()` 就是一串 `[native code]`。
    expect(proxyErrorReason({ errorCode: 'toString', message: 'm' }, t)).toBe('m');
    expect(proxyErrorReason({ errorCode: 'constructor', message: 'm' }, t)).toBe('m');
  });

  it('通用兜底句本身五语齐备（第 3 段不许也回落成裸键名）', () => {
    for (const l of SUPPORTED_LANGUAGES) {
      const v = LOCALES[l]['errors.operationFailed'];
      expect(typeof v === 'string' && v.trim() !== '', `${l} 缺 errors.operationFailed`).toBe(true);
    }
  });
});
