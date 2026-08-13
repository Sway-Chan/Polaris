/**
 * config 内容版本（`baseVersion`）的**跨语言值锁** —— 前端这一半。
 *
 * # 这道门守的是什么
 *
 * `config:save` 的乐观并发校验（spec §2.5 Q8-b）走「两侧各算，不走 IPC 往返」：前端对
 * `config:get` 拿到的 config 算 `configBaseVersion`，Rust 对磁盘现值算 `config_content_hash`。
 * 两侧一旦分叉，**每一次带 `baseVersion` 的保存都会返 conflict** —— 功能整体失效，而且失效方式
 * 是「保存按钮点了没反应」，从任何一侧单独看都像对方的 bug。
 *
 * # 为什么是固定 fixture 双侧计算，而不是读源码对拍
 *
 * 本仓 `staged-classification.test.ts` / `user-config-fields.test.ts` 锁的是**表一致性**
 * （枚举成员集、字段名集），解析 Rust 源码取真值是对的。本条锁的是**值一致性**：
 * 「同一份输入两边算出同一个数」这件事，只能靠双方各自跑一遍再和同一个写死的期望值比。
 * 比源码文本在这里毫无意义 —— 两份实现长得完全不一样（`Math.imul` vs `wrapping_mul`、
 * `charCodeAt` vs `encode_utf16`），文本相同反而不可能。
 *
 * Rust 那一半：`src-tauri/src/commands/config.rs` 的
 * `config_version_matches_the_shared_cross_language_fixture`，读的是同一个 fixture 文件。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { configBaseVersion } from '@/lib/staged-config';

interface FixtureCase {
  name: string;
  why: string;
  expected: string;
  config: unknown;
}

const FIXTURE = JSON.parse(
  readFileSync(fileURLToPath(new URL('./config-version.fixture.json', import.meta.url)), 'utf8')
) as { cases: FixtureCase[] };

/** 按名取 case；取不到即抛（fixture 被改名/删条目时**转红**，而不是让断言凭空消失）。 */
function caseNamed(name: string): FixtureCase {
  const hit = FIXTURE.cases.find((c) => c.name === name);
  if (hit === undefined) {
    throw new Error(
      `fixture 里没有 case \`${name}\` —— 要么它被改名/删了（那么本门该跟着改），` +
        '要么 fixture 路径写错了；两种情况都不允许静默放行。'
    );
  }
  return hit;
}

describe('configBaseVersion 跨语言值锁', () => {
  /**
   * 自曝纪律：读空 / 读少了必须转红，而不是「0 个用例全绿」。
   * 「没检查」与「检查通过」的输出不可区分 = 没有这道门。
   */
  it('fixture 真的读到了（自曝：空集合不得恒绿）', () => {
    expect(FIXTURE.cases.length).toBeGreaterThanOrEqual(8);
  });

  /**
   * 主断言：每条 fixture 的前端计算值 === 写死的 `expected`（Rust 侧对同一个数断言）。
   *
   * 牙（前端侧）：把 `charCodeAt` 换成 `codePointAt` → `nonAscii` 转红（emoji 是代理对，
   * 两者取值不同）；把 `Math.imul` 换成 `*` → 全部转红（JS 双精度乘会丢低位）；
   * 把 `stableStringify` 的 `.sort()` 去掉 → `nestedKeysShuffled` 转红。
   * 牙（Rust 侧）：见 `config.rs` 同名测试的文档。
   */
  it.each(FIXTURE.cases.map((c) => [c.name, c] as const))(
    'case %s 的短 hash 与 Rust 侧一致',
    (_name, c) => {
      expect(configBaseVersion(c.config)).toBe(c.expected);
    }
  );

  /**
   * fixture 内部的两条**语义**不变式（不是 hash 值本身，是它该有的性质）。
   * 写在这里而不是只靠 `expected`：若哪天有人整体重算了一批 expected 来「修绿」，
   * 这两条仍会拦住「键序敏感」和「数组被排序」这两类真错。
   */
  it('键序无关：同一份配置打乱键序后同 hash', () => {
    expect(configBaseVersion(caseNamed('nested').config)).toBe(
      configBaseVersion(caseNamed('nestedKeysShuffled').config)
    );
  });

  it('数组保序：同元素不同顺序必须不同 hash', () => {
    expect(configBaseVersion(caseNamed('arrayOrder').config)).not.toBe(
      configBaseVersion(caseNamed('arrayOrderSwapped').config)
    );
  });
});
