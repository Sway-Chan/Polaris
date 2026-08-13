/**
 * Rust 侧 i18n 的**跨语言覆盖门** —— 前端语言集 ↔ Rust `Lang` ↔ `locales/auxiliary/` 的 `native.*`。
 *
 * # 这道门守的是什么
 *
 * 2026-07-31 之前 Rust 侧**没有** i18n：原生文件对话框、提权引导消息框、应用菜单一律硬编码中文，
 * 托盘原生菜单/tooltip 只有 zh/en 二态。修法是让 Rust 经 `src-tauri/src/i18n.rs` 读**同一批**
 * locale 文件（`ui/src/i18n/locales/auxiliary/*.json`），于是 aux 分区从「两个辅助 webview」
 * 扩成「两个辅助 webview + Rust 进程」。这带来三条新的、**全部静默**的漂移面：
 *
 *  1. **语种集分叉**：`domain/language.ts` 的 `SUPPORTED_LANGUAGES` 加了一个语种，
 *     `i18n.rs` 的 `Lang` / `SUPPORTED` 没跟 ⇒ 该语种用户的界面切过去了，但原生对话框、
 *     托盘菜单、提权框全部回落英文。前端一侧毫无异常，Rust 一侧编译通过。
 *  2. **`native.*` 被 TS 侧消费**：`native.*` 的唯一消费方是 Rust 进程。主窗 i18next
 *     **没加载** aux 分区（见 `i18n/index.ts` 的 resources），辅助窗只具名导入
 *     `tray` / `updatePopup` 两棵子树 ⇒ 谁在 TS 里写 `t('native.x')` 都只会渲染出裸键名，
 *     且没有任何现成的门会红（`i18n-coverage.test.ts` 的 G4 此前只登记了 `tray.` / `updatePopup.`）。
 *  3. **Rust 引用的 `tray.*` 键被删**：Rust 与浮层**共用** `tray.*`（同一概念不得在两个入口
 *     措辞分叉）。浮层那边删掉一个键、locale 也跟着删，Rust 侧的 `key::TRAY_*` 常量就成了
 *     悬空引用 —— 运行期回落成键名显示在托盘菜单上，`tsc` 与 `cargo build` 都不会说话。
 *
 * 前两条在 Rust 侧无从判定（Rust 看不见 TS 源码与前端语种表），第三条在 TS 侧无从判定
 * （TS 看不见 Rust 的键常量）。故必须有一道**两边源码都读**的门，这就是本文件。
 *
 * Rust 一侧对称的那半边在 `src-tauri/src/i18n.rs` 的测试里（键在五语种齐备 / `native.*`
 * 无死键 / 用户可见 sink 不得裸写中文），两边合起来才是完整射程。
 *
 * # 为什么读源码而不是抄一份镜像常量
 *
 * 抄镜像只是把漂移面往后挪一格（改了源不改镜像 = 门在守一个假真值）。范式照抄同目录的
 * `app-language-coverage.test.ts` / `protocol-settings-coverage.test.ts`：把 Rust 源码当真值读进来解析。
 *
 * # 自曝纪律
 *
 * 任何一处解析不出内容一律 **throw**，不走「读不到就跳过」—— 那样常量一改名门就静默消失，
 * 「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 */

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

import { SUPPORTED_LANGUAGES } from '../domain/language';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

const RUST_I18N = read('../../../src-tauri/src/i18n.rs');
const AUX_DIR = fileURLToPath(new URL('../i18n/locales/auxiliary', import.meta.url));
const SRC_DIR = fileURLToPath(new URL('..', import.meta.url));

/** Rust `Lang::code()` 的分支 → 语言码。形变即抛。 */
function rustLangCodes(): string[] {
  const block = /pub const fn code\(self\) -> &'static str \{\s*match self \{([\s\S]*?)\}/.exec(
    RUST_I18N,
  );
  if (!block) {
    throw new Error(
      'i18n.rs 里找不到 `Lang::code()` 的 match —— 语种表被改名或改形了，本门已失去判据',
    );
  }
  const codes = [...block[1].matchAll(/Lang::\w+\s*=>\s*"([\w-]+)"/g)].map((m) => m[1]);
  if (codes.length === 0) throw new Error('`Lang::code()` 里一条分支都没解析到');
  return codes;
}

/** Rust `mod key` 里声明的全部键。形变即抛。 */
function rustDeclaredKeys(): string[] {
  const start = RUST_I18N.indexOf('pub mod key {');
  if (start < 0) throw new Error('i18n.rs 里找不到 `pub mod key {` —— 本门已失去判据');
  const body = RUST_I18N.slice(start);
  const end = body.indexOf('\n}\n');
  if (end < 0) throw new Error('`mod key` 的收尾锚点消失 —— 本门已失去判据');
  const keys = [...body.slice(0, end).matchAll(/pub const \w+: &str = "([\w.]+)";/g)].map(
    (m) => m[1],
  );
  if (keys.length < 30) throw new Error(`只解析到 ${keys.length} 个键常量 —— 写法变了？`);
  return keys;
}

const flatten = (o: unknown, p = '', out: Record<string, string> = {}) => {
  for (const [k, v] of Object.entries(o as Record<string, unknown>)) {
    const kk = p ? `${p}.${k}` : k;
    if (typeof v === 'string') out[kk] = v;
    else if (v && typeof v === 'object') flatten(v, kk, out);
  }
  return out;
};

const AUX: Record<string, Record<string, string>> = Object.fromEntries(
  SUPPORTED_LANGUAGES.map((l) => [
    l,
    flatten(JSON.parse(readFileSync(join(AUX_DIR, `${l}.json`), 'utf8'))),
  ]),
);

describe('Rust 侧 i18n 与前端的三向对账', () => {
  it('Rust `Lang` 的语种集 = 前端 `SUPPORTED_LANGUAGES`', () => {
    // 缺一项的症状：该语种用户界面切过去了，原生对话框/托盘/提权框全落英文，无任何报错。
    expect([...rustLangCodes()].sort(), 'Rust `Lang::code()` 与前端语种表分叉').toEqual(
      [...SUPPORTED_LANGUAGES].sort(),
    );
  });

  it('Rust 引用的每个键在五份 aux locale 里都有真译文', () => {
    // 与 `i18n.rs` 内那条同向的断言互为冗余是**刻意**的：Rust 侧那条守「改 Rust 时别漏译」，
    // 这条守「改 locale 时别把 Rust 在用的键删了/挪了」——后者改的是 TS 仓这边的文件，
    // 改的人未必会跑 cargo test。
    const missing: string[] = [];
    for (const k of rustDeclaredKeys()) {
      for (const l of SUPPORTED_LANGUAGES) {
        const v = AUX[l][k];
        if (typeof v !== 'string' || v.trim() === '') missing.push(`${l}: ${k}`);
      }
    }
    expect(missing.sort(), 'Rust 侧在用的键没有五语种齐备（补进 locales/auxiliary/*.json）').toEqual([]);
  });

  it('`native.*` 只许 Rust 消费，TS 侧一处都不许出现', () => {
    // 主窗 i18next 不加载 aux；辅助窗只具名导入 tray / updatePopup 两棵子树。
    // ⇒ TS 里写这个命名空间的键必然渲染出裸键名，而没有任何现成的门会红。
    //
    // **测试/规格文件排除在外**，与 `i18n/i18n-coverage.test.ts::collectSources()` 同口径：
    // 它们不渲染任何界面，且门自己的说明文字里会**逐字引用**被禁的写法当反例
    // （本条第一版就因此红了，红在 `i18n-coverage.test.ts` 的一段注释上 —— 是真阳性的
    // 判定逻辑打在了假阳性的目标上）。代价：`.test.ts` 里写这个键不会被抓，无实害。
    const bad: string[] = [];
    const walk = (dir: string) => {
      for (const e of readdirSync(dir)) {
        if (e === 'node_modules' || e === 'dist') continue;
        const full = join(dir, e);
        if (statSync(full).isDirectory()) walk(full);
        else if (/\.tsx?$/.test(e) && !/\.(test|spec)\.tsx?$/.test(e)) {
          for (const m of readFileSync(full, 'utf8').matchAll(/\bt\(\s*'(native\.[\w]+)'/g)) {
            bad.push(`${full.slice(SRC_DIR.length)}: ${m[1]}`);
          }
        }
      }
    };
    walk(SRC_DIR);
    expect(bad.sort(), '`native.*` 被 TS 侧消费（它只由 Rust 进程渲染）').toEqual([]);
  });

  it('自检：语料与解析都非空（防「扫到 0 条于是全绿」）', () => {
    expect(Object.keys(AUX).length, 'aux 语种不齐').toBe(5);
    for (const l of SUPPORTED_LANGUAGES) {
      expect(Object.keys(AUX[l]).length, `${l} 的 aux 语料为空`).toBeGreaterThan(50);
    }
    expect(rustDeclaredKeys().length).toBeGreaterThanOrEqual(30);
    expect(rustLangCodes().length).toBe(5);
  });
});
