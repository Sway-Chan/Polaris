/**
 * `CORE_STILL_RUNNING_CODES` 的**跨语言锁** —— 前端「哪些 `event:proxyError` 不算重启失败」
 * ↔ Rust `runtime/proxy.rs` 里真正走 `set_nonfatal_error` 的那几个码。
 *
 * # 这道门守的是什么
 *
 * `isRestartFailureCode` 取的是**补集**（不在表里的一律算失败）。这个方向本身是对的
 * ——漏判一个失败码会让条永远停在「应用中…」、用户没有出口；漏判一个非终态码只多一次可点掉的红。
 * 但补集判据有一个单侧失效模式：**Rust 新增一个非终态码而前端表不动**，于是一次核仍在跑的
 * 非致命告警被算成「本次立即应用的重启失败了」，条转红、显示一条与事实相反的话。
 *
 * TypeScript 抓不到它（`errorCode` 运行期就是个 string），任何前端测试也抓不到
 * ——前端表自己跟自己比永远一致。只有把 Rust 源码当真值读进来才有信息量。
 *
 * # 判据取自注释而非调用点
 *
 * `set_nonfatal_error` 的调用点传的是 `code::XXX` 常量引用，从调用点反推码字面量要跨两跳；
 * 而 `code` 模块里每个非终态码的文档注释都逐条写着「非终态」+ `set_nonfatal_error`。
 * 直接扫那段注释块，锚点更稳、也正是维护者写下判据的地方。
 *
 * # 自曝纪律
 *
 * 抓不到 `mod code` 段必须**抛**，而不是拿到空集合让断言恒真。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { isRestartFailureCode } from './pending-bar-logic';

const RUST_PROXY = readFileSync(
  fileURLToPath(new URL('../../../../src-tauri/src/runtime/proxy.rs', import.meta.url)),
  'utf8'
);

/**
 * 从 `code` 模块里抽出所有**被注释标为非终态**的码字面量。
 *
 * 做法：把模块体按 `pub const` 切成「每个常量 + 它上面的文档注释」的块，
 * 注释里出现「非终态」的那些块就是目标。
 */
function nonfatalCodesInRust(): Set<string> {
  const start = RUST_PROXY.indexOf('pub mod code {');
  if (start < 0) {
    throw new Error(
      'proxy.rs 里找不到 `pub mod code {` —— 锚点失配。要么模块被改名/搬走（那么本门该跟着改），' +
        '要么本解析器坏了；两种情况都不允许静默放行。'
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
  const body = RUST_PROXY.slice(bodyStart, end);

  const out = new Set<string>();
  let pending: string[] = [];
  for (const line of body.split('\n')) {
    const trimmed = line.trim();
    if (trimmed.startsWith('///') || trimmed.startsWith('//')) {
      pending.push(trimmed);
      continue;
    }
    const m = /pub const [A-Z_]+: &str = "([A-Z_]+)";/.exec(trimmed);
    if (m !== null) {
      if (pending.some((c) => c.includes('非终态'))) out.add(m[1]);
      pending = [];
      continue;
    }
    // 常量与常量之间的空行不打断注释块（rustfmt 不会在 doc 注释与其常量之间插空行）。
    if (trimmed !== '') pending = [];
  }
  return out;
}

describe('非终态错误码跨语言锁', () => {
  it('Rust 侧标为「非终态」的码集合 = 前端 CORE_STILL_RUNNING_CODES', () => {
    const rust = nonfatalCodesInRust();
    expect(rust.size, '一个都没解析到 = 这道门没在检查任何东西').toBeGreaterThan(0);
    // 前端表没有导出（有意：它是 isRestartFailureCode 的实现细节），故经谓词反查。
    const frontendTreatsAsStillRunning = [...rust].filter((c) => !isRestartFailureCode(c));
    expect(
      [...frontendTreatsAsStillRunning].sort(),
      'Rust 新增/删除了非终态码而前端表没跟 —— 核仍在跑的告警会被说成「应用失败」'
    ).toEqual([...rust].sort());
  });

  it('终态码一律算重启失败（补集方向不得反过来）', () => {
    // 这几个在 Rust 侧明确走 set_error（核未起）。变异对照：把它们塞进前端表 → 转红，
    // 且真实后果是条永远停在「应用中…」——用户没有任何出口。
    for (const code of ['STARTUP_FAILED', 'PROCESS_EXITED', 'TUN_ROUTE_NOT_CAPTURED']) {
      expect(isRestartFailureCode(code), `${code} 是终态，必须算失败`).toBe(true);
    }
  });

  it('errorCode 缺失时按失败处理（判不出来不许乐观）', () => {
    expect(isRestartFailureCode(undefined)).toBe(true);
  });
});
