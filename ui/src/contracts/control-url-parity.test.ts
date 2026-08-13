/**
 * `control_url` 判据的**跨语言对拍门**。
 *
 * 前端 `domain/control-url.ts` 与 Rust `user_config/control_url.rs` 是同一判据的两份实现：
 * 前者拦在**保存那一刻**（光标还在输入框旁边），后者拦在**下发到核之前**（兼管订阅/手改 JSON
 * 来的配置）。两份都必要，但两份就会漂。
 *
 * 这道门不抄镜像常量 —— 它**把 Rust 单测里的 URL 语料表读进来**当真值跑前端实现。
 * Rust 侧加了一行新形态而前端没跟上 ⇒ 本门红；Rust 侧改了归类 ⇒ 本门红。
 *
 * # 为什么语料在 Rust 那边
 *
 * 因为那边的每一行都标着「实测 PANIC / 实测 FATAL」，是用真内核（`resources/linux/sing-box`，
 * 1.14.0-beta.3）逐条 `sing-box check` 出来的。真值只该有一处，另一处引用它。
 *
 * # 这门抓不到什么
 *
 * - **判据本身对不对**：它只保证两份实现一致。两份一起错，它全绿。那由 Rust 侧语料的实测标注负责。
 * - **运行期语义**：`sing-box check` 通过不等于 tsnet 真能用那个控制面（check 只 `New()` 不 `Start()`）。
 * - 前端的**调用点**是否真在保存前拦（那由 `TsSettingsDialog` 自身的用例守）。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { controlUrlReject, type ControlUrlReject } from '@/domain/control-url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));
const RUST_SRC = join(REPO_ROOT, 'crates/config-engine/src/user_config/control_url.rs');

/** Rust 测试函数名 → 该函数里每条 URL 的期望判定（`null` = 应放行）。 */
const EXPECTED_BY_FN: Readonly<Record<string, ControlUrlReject | null>> = {
  ip_literal_forms_all_rejected: 'control-url-ip',
  ipv6_forms_all_rejected: 'control-url-ip',
  missing_scheme_rejected: 'control-url-scheme',
  domain_forms_never_rejected: null,
};

/** 取某个 `#[test] fn <name>` 的函数体（到下一个 `    #[test]` 或 `mod` 结束为止）。 */
function rustFnBody(src: string, fn: string): string {
  const start = src.indexOf(`fn ${fn}()`);
  if (start < 0) return '';
  const rest = src.slice(start);
  const next = rest.indexOf('\n    #[test]');
  return next < 0 ? rest : rest.slice(0, next);
}

/**
 * 砍掉行尾注释 —— 只砍**字符串外**的 `//`。
 *
 * 不能用 `line.split('//')[0]`：本表的语料本身就是 URL（`http://…`），那样一刀把每条都砍没，
 * 抽取器返回空、而下面每条断言都变成对空集恒真 —— 阳性对照抓的就是这个（我第一版正是这么错的）。
 */
function stripLineComment(line: string): string {
  let inStr = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (inStr) {
      if (c === '\\') i++;
      else if (c === '"') inStr = false;
    } else if (c === '"') inStr = true;
    else if (c === '/' && line[i + 1] === '/') return line.slice(0, i);
  }
  return line;
}

/** 抽出函数体里 `for url in [...]` 那张表的字符串字面量（注释里的不算）。 */
function urlsIn(body: string): string[] {
  const out: string[] = [];
  for (const line of body.split('\n')) {
    const code = stripLineComment(line);
    for (const m of code.matchAll(/"((?:[^"\\]|\\.)*)"/g)) {
      const raw = m[1] ?? '';
      // 只要看起来像 URL / host 的行，跳过 assert 里的中文提示串。
      if (raw !== '' && !/[一-鿿]/.test(raw)) out.push(raw.replace(/\\"/g, '"'));
    }
  }
  return out;
}

const SRC = readFileSync(RUST_SRC, 'utf8');

describe('control_url 判据跨语言对拍', () => {
  it('Rust 语料表读得到（阳性对照：读空 ⇒ 下面每条断言都恒真）', () => {
    expect(SRC.length, 'Rust 源码读不到，路径可能变了').toBeGreaterThan(1000);
    for (const fn of Object.keys(EXPECTED_BY_FN)) {
      const urls = urlsIn(rustFnBody(SRC, fn));
      expect(urls.length, `Rust 测试 ${fn} 里抽不到任何 URL 语料 —— 抽取器失效，本门会恒绿`).
        toBeGreaterThan(0);
    }
  });

  for (const [fn, expected] of Object.entries(EXPECTED_BY_FN)) {
    it(`${fn}：前端判定与 Rust 语料一致`, () => {
      const urls = urlsIn(rustFnBody(SRC, fn));
      const mismatched = urls
        .map((u) => ({ url: u, got: controlUrlReject(u) }))
        .filter((r) => r.got !== expected);
      expect(
        mismatched,
        `前端 controlUrlReject 与 Rust 语料分叉（期望 ${String(expected)}）—— ` +
          '两份实现漂了，保存前拦的和下发前拦的不是同一套判据'
      ).toEqual([]);
    });
  }

  it('刻意偏严的两条也一致（核接受、我们拦）', () => {
    // 与 Rust `intentionally_stricter_than_upstream` 同源；这两条**核实测通过**，
    // 前端若放行就会与后端分叉（用户在弹窗里存得下去、下发时却被剔掉）。
    expect(controlUrlReject('http://192.168.001.010:8080')).toBe('control-url-ip');
    expect(controlUrlReject('//hs.example.com')).toBe('control-url-scheme');
  });

  it('空值放行（没填 = 用官方 controlplane）', () => {
    expect(controlUrlReject('')).toBeNull();
    expect(controlUrlReject('   ')).toBeNull();
    expect(controlUrlReject(null)).toBeNull();
    expect(controlUrlReject(undefined)).toBeNull();
  });
});
