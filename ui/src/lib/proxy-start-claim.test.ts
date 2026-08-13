/**
 * 起核认领闸门单测 —— 锁死「提权门两码只报一遍，且无人 await 的入口不得因此变静默」。
 *
 * 两个方向的失效都要钉住（缺一即半个门）：
 *  - 抑制不足 ⇒ 退回双报（事件腿 + await 腿各弹一次，NOT_INSTALLED 更是三重）；
 *  - 抑制过度 ⇒ 托盘 / 启动自动连接 / switchMode 去抖重启这些**没人 await** 的入口失败后完全静默
 *    （真机反馈「点了没反应」，正是第二批要修的病）。
 */
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import {
  withProxyStartClaim,
  isProxyStartClaimed,
  resetProxyStartClaim,
} from './proxy-start-claim';

describe('withProxyStartClaim / isProxyStartClaimed（起核认领闸门）', () => {
  beforeEach(() => {
    resetProxyStartClaim();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    resetProxyStartClaim();
  });

  it('无人认领时不认领 —— 托盘/自动连接入口必须落在这一态（否则事件腿静默）', () => {
    expect(isProxyStartClaimed()).toBe(false);
  });

  it('调用在飞期间即认领（覆盖「事件早于 promise 落定到达」的顺序）', async () => {
    let seen: boolean | undefined;
    await withProxyStartClaim(async () => {
      seen = isProxyStartClaimed();
    });
    expect(seen).toBe(true);
  });

  it('落定后宽限期内仍认领（覆盖「事件晚于 promise reject 到达」的顺序）', async () => {
    await expect(
      withProxyStartClaim(() => Promise.reject(new Error('HELPER_GATE_ABORTED')))
    ).rejects.toThrow('HELPER_GATE_ABORTED');
    // reject 落定的一瞬间 depth 已归零，此刻仍认领 = 宽限尾巴生效；没有它这里就漏 ⇒ 双报。
    expect(isProxyStartClaimed()).toBe(true);
    vi.advanceTimersByTime(1999);
    expect(isProxyStartClaimed()).toBe(true);
  });

  it('resolve 路径同样留宽限尾巴', async () => {
    await withProxyStartClaim(async () => 'ok');
    expect(isProxyStartClaimed()).toBe(true);
  });

  it('宽限期过后解除认领 —— 认领不得长期挂住（否则后续托盘失败被永久吞掉）', async () => {
    await withProxyStartClaim(async () => {});
    vi.advanceTimersByTime(2001);
    expect(isProxyStartClaimed()).toBe(false);
  });

  it('透传 resolve 值与 reject —— 发起方仍靠 await 腿自己 catch 并提示', async () => {
    await expect(withProxyStartClaim(async () => 42)).resolves.toBe(42);
    const boom = new Error('boom');
    await expect(withProxyStartClaim(() => Promise.reject(boom))).rejects.toBe(boom);
  });
});

/* ────────────────────────────────────────────────────────────────────────────
 * 消费面守卫
 *
 * 上面那组只能证明闸门本身对，证明不了**发起方真的在认领**。本仓 vitest 是 node 环境（无 jsdom/
 * testing-library），组件渲染不了；若哪天有人把 `withProxyStartClaim` 那层包裹删掉，上面 6 条
 * 会全绿而双报缺陷复活（射程 ≠ 批次范围）。故补一条扫源码的守卫，把「任何 `startProxy()` 调用
 * 必须被认领包裹」这条结构约束钉死。
 *
 * **扫描面是整个 `ui/src`，不是单个 HomeScreen.tsx**：早先只扫 HomeScreen 一个文件，对**那一个**
 * 文件有牙 —— 但新增组件（快连卡片 / 命令面板 / 托盘浮层）里再写一处 `startProxy()`，守卫压根看不见
 * ⇒ 那条腿未认领 ⇒ 双报复活。守卫的射程必须等于约束的射程，而不是等于「当初写它时恰好有的那个文件」。
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 去注释后再扫 —— 守卫针对的是**代码**，注释里讲解这条约束（HomeScreen / proxy-start-claim.ts 里
 * 就在讲）不该算违规。
 *
 * **为什么是字符扫描而不是两条正则**：`/\/\*[\s\S]*?\*\//g` 不认字符串边界，会把**字符串字面量里的**
 * `/*` 当成注释起点，非贪婪吃到下一个 `*​/` —— 中间夹着的真代码被一并删掉 ⇒ 违规从此看不见（假阴性，
 * 守卫恒绿）。本扫描器带一个「是否在字符串里」的状态位，只摘代码位置上的注释。
 * 失败方向刻意选「响」而非「哑」，理由同 `settings-logic.test.ts` 的同名函数。
 */
function stripComments(src: string): string {
  let out = '';
  let quote: string | null = null; // 当前所处字符串的引号（' " `）；null = 在代码里
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const next = src[i + 1];
    if (quote !== null) {
      if (c === '\\') {
        out += c + (next ?? ''); // 转义对整体保留，避免 \" 被误判为字符串结束
        i += 2;
        continue;
      }
      if (c === quote) quote = null;
      out += c;
      i++;
      continue;
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c;
      out += c;
      i++;
      continue;
    }
    if (c === '/' && next === '*') {
      const end = src.indexOf('*/', i + 2);
      i = end === -1 ? src.length : end + 2;
      out += ' '; // 留一个空白，避免把注释两侧的 token 粘成一个
      continue;
    }
    if (c === '/' && next === '/') {
      const end = src.indexOf('\n', i + 2);
      i = end === -1 ? src.length : end;
      out += ' ';
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

/** 认领包裹的容错形（允许空白/换行差异，但要求确实是「包住 startProxy() 调用」）。 */
const WRAPPED = /withProxyStartClaim\(\s*\(\s*\)\s*=>\s*startProxy\(\s*\)\s*\)/g;

/** 裸起核调用。`store` 里的**声明**（`startProxy: () => Promise<void>` / `startProxy: async () =>`）
 *  形态是 `startProxy:`，不匹配本式 —— 守的是**调用点**，不是定义点。 */
const CALL = /startProxy\(\s*\)/;

describe('消费面守卫 —— 发起方必须认领', () => {
  it('ui/src 内任何 startProxy() 调用一律经 withProxyStartClaim 包裹', async () => {
    const fs = await import('node:fs');
    const path = await import('node:path');
    const { fileURLToPath } = await import('node:url');

    // `ui/src` 根（本文件在 `ui/src/lib/`）。
    const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
    // 扫描根漂走时必须**红**，而不是静静扫了个空 —— 扫不到源码的守卫形同虚设。
    expect(fs.existsSync(root)).toBe(true);

    /**
     * 递归收集 `ui/src` 下的生产 `.ts(x)`（相对 root 的路径）。
     * 排除 `*.test.ts(x)` / `*.spec.ts(x)`：测试里的违规样本是**字符串字面量 / 正则**
     * （stripComments 摘不掉），扫它等于自己判自己违规。
     */
    const collect = (dir: string): string[] =>
      fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
        const full = path.join(dir, e.name);
        if (e.isDirectory()) return collect(full);
        if (!/\.tsx?$/.test(e.name) || /\.(test|spec)\.tsx?$/.test(e.name)) return [];
        return [path.relative(root, full)];
      });

    const scanned = collect(root);

    // ── 扫描面自检（非空 + 不缩水）──
    // 光 `> 0` 挡得住「全塌」，挡不住「递归被改坏只剩顶层」：故同时钉住**必扫锚点**。
    expect(scanned.length).toBeGreaterThanOrEqual(30);
    // 锚点 = 当前唯一的起核发起方所在文件（在子目录里，递归一断它就消失）+ 闸门自身所在文件。
    expect(scanned).toEqual(
      expect.arrayContaining([
        path.join('components', 'screens', 'home', 'HomeScreen.tsx'),
        path.join('lib', 'proxy-start-claim.ts'),
      ]),
    );

    const files = scanned.map((rel) => ({
      rel,
      code: stripComments(fs.readFileSync(path.join(root, rel), 'utf8')),
    }));

    // 前提自检：本守卫的整个意义建立在「仓里确实存在起核调用」之上。全仓一处调用都扫不到 =
    // 要么调用被挪去了扫描面外，要么 CALL 式失配 —— 两种都必须响，不能静静判「无违规」。
    expect(files.filter((f) => CALL.test(f.code)).length).toBeGreaterThan(0);

    // 把合规写法整段摘掉后，全仓不得再有裸的 startProxy() —— 既拦「包裹被删」，
    // 也拦「在任何新文件里新增了一处没包的调用」。
    const offenders = files
      .filter((f) => CALL.test(f.code.replace(WRAPPED, '')))
      .map((f) => f.rel);
    expect(offenders).toEqual([]);
  });

  it('守卫本身有牙：裸调用判违规、包裹后放行', () => {
    // 反向自检 —— 防止 stripComments 写过头（把源码吃空）或 WRAPPED 写太宽（把裸调用也摘掉）导致恒绿。
    expect(CALL.test(stripComments('await startProxy();').replace(WRAPPED, ''))).toBe(true);
    expect(
      CALL.test(stripComments('await withProxyStartClaim(() => startProxy());').replace(WRAPPED, '')),
    ).toBe(false);
    // 注释里的调用不算违规；**字符串里的 `/*` 不得吃掉后面的真代码**（旧正则版正是栽在这里）。
    expect(CALL.test(stripComments('// await startProxy();'))).toBe(false);
    expect(CALL.test(stripComments('const s = "/*"; await startProxy(); const e = "*/";'))).toBe(true);
  });
});
