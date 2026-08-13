/**
 * 「核没跑时绕开暂存」的判据门（选项 (d)）—— 陈先生原话：停核期间的改动是否应该直接生效。
 *
 * 落法：`活跃 := 总开关开 ∧ ¬(核没在跑 ∧ 没有已暂存条目)`，即 `stagingActive`。
 *
 * 本文件两组门，是**分辨落的是 (d) 还是 (b) 的唯一判据**：
 *  1. 穷举 `coreRunning × entries 空/非空 × 改动类型` —— 改动类型这一维经 `editRoute` 复合，
 *     因为「活跃」只是它的 `enabled` 实参，真正落地的是两者的合成结果。
 *  2. **核没跑但有暂存条目时仍走暂存** —— (b) 会让新改动直写、老条目留在暂存里（分裂态），
 *     (d) 不会。没有这一组就分不出实现的是哪一个。
 */
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { describe, it, expect } from 'vitest';
import { editRoute, stagingActive } from './staged-config';

/** 三类 key 各取一个代表（分类依据见 `editRoute` 的两张表）。 */
const KEY = {
  /** 正常进暂存的核心键。 */
  staged: 'bypassLANList',
  /** W-1 绕过键（切出口即时生效）。 */
  bypassed: 'selectedServerId',
  /** W-0 豁免键（不在 29 个 UserConfig 字段里）。 */
  exempt: 'ghProxyPrefix',
} as const;

describe('stagingActive —— 穷举 开关 × coreRunning × entries', () => {
  // 总开关关 ⇒ 恒不活跃，另两维不参与（关掉开关的行为等价性押在这条上）。
  // 变异对照：把 `if (!enabled) return false` 删掉 → 本条转红。
  it('总开关关 ⇒ 恒 false（另两维无关）', () => {
    for (const running of [true, false]) {
      for (const n of [0, 1, 5]) {
        expect(stagingActive(false, running, n), `running=${running} n=${n}`).toBe(false);
      }
    }
  });

  // 核在跑 ⇒ 恒活跃（今天的行为，一字不变）。
  // 变异对照：把判据写成 `stagedCount > 0`（丢掉 coreRunning 那条腿）→ 本条 n=0 那格转红，
  // 即「核在跑、手上没暂存条目时改一条」会被直写入核 = 每改一条断一次流，正是暂存层要消除的。
  it('核在跑 ⇒ 恒 true（无论有没有暂存条目）', () => {
    for (const n of [0, 1, 5]) {
      expect(stagingActive(true, true, n), `n=${n}`).toBe(true);
    }
  });

  // 核没跑 + 没有暂存条目 ⇒ 这一格、且**只有**这一格绕开暂存。
  // 变异对照：把 `coreRunning || stagedCount > 0` 改成 `true` → 本条转红（(d) 退回今天行为）。
  it('核没跑 + 无暂存条目 ⇒ false（唯一直写格）', () => {
    expect(stagingActive(true, false, 0)).toBe(false);
  });

  /**
   * **(d) 与 (b) 的分界**：核没跑但**有**暂存条目 ⇒ 仍走暂存。
   *
   * 少了这一条就是 (b)：新改动直写、老条目留在暂存里 ⇒ 分裂态。而那批条目是相对 `baseline`
   * 建立的，直写把盘从它底下抽走，之后保存那批会因**用户自己造成的改动**弹冲突窗。
   *
   * 变异对照：把判据改成 `coreRunning`（丢掉 `stagedCount > 0` 那条腿）→ 本条转红。
   */
  it('核没跑 + 有暂存条目 ⇒ true（仍走暂存，不产生分裂态）', () => {
    for (const n of [1, 2, 17]) {
      expect(stagingActive(true, false, n), `n=${n}`).toBe(true);
    }
  });
});

describe('与 editRoute 合成 —— 穷举 coreRunning × entries × 改动类型', () => {
  const MATRIX: ReadonlyArray<
    readonly [coreRunning: boolean, stagedCount: number, key: string, want: 'staged' | 'direct']
  > = [
    // 核在跑：与今天逐字节相同 —— 核心键进暂存，绕过键与豁免键直落盘。
    [true, 0, KEY.staged, 'staged'],
    [true, 0, KEY.bypassed, 'direct'],
    [true, 0, KEY.exempt, 'direct'],
    [true, 3, KEY.staged, 'staged'],
    [true, 3, KEY.bypassed, 'direct'],
    [true, 3, KEY.exempt, 'direct'],
    // 核没跑 + 无暂存条目：**核心键也直落盘**（这就是 (d) 的全部改动面）。
    [false, 0, KEY.staged, 'direct'],
    [false, 0, KEY.bypassed, 'direct'],
    [false, 0, KEY.exempt, 'direct'],
    // 核没跑 + 有暂存条目：核心键回到暂存（(d) 与 (b) 的分界，见上一组）。
    [false, 2, KEY.staged, 'staged'],
    [false, 2, KEY.bypassed, 'direct'],
    [false, 2, KEY.exempt, 'direct'],
  ];

  it('12 格逐格钉住', () => {
    for (const [running, n, key, want] of MATRIX) {
      const got = editRoute(key, stagingActive(true, running, n));
      expect(got, `coreRunning=${running} entries=${n} key=${key}`).toBe(want);
    }
  });

  // 绕过键 / 豁免键的去向**不因运行态而变** —— 它们本来就直落盘，(d) 不该给它们引入任何新分支。
  // 变异对照：把 `stagingActive` 的返回值反过来喂（活跃时直写）→ 本条转红。
  it('绕过键与豁免键在四种运行态组合下恒 direct', () => {
    for (const key of [KEY.bypassed, KEY.exempt]) {
      for (const running of [true, false]) {
        for (const n of [0, 2]) {
          expect(editRoute(key, stagingActive(true, running, n)), `${key} ${running} ${n}`).toBe(
            'direct'
          );
        }
      }
    }
  });
});

/**
 * **接线守卫** —— 判据对 ≠ 判据被调用。`config-write-wiring.test.ts` 那份登记表守的是「写入口有没有
 * 分过类」，本组守的是**分类时喂进去的那个布尔取自哪里**：任一编辑入口直接读 store 的 `enabled`，
 * 它在核没跑时就还会走暂存 —— (d) 对那条入口静默失效，而所有纯函数门仍绿。
 */
describe('接线：编辑入口喂给 editRoute 的布尔必须来自 useStagingActive', () => {
  const SRC = new URL('..', import.meta.url).pathname; // ui/src/

  function sources(dir = SRC, out: string[] = []): string[] {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) sources(p, out);
      else if (/\.(ts|tsx)$/.test(e.name) && !e.name.includes('.test.')) out.push(p);
    }
    return out;
  }
  const stripComments = (s: string): string =>
    s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^[ \t]*\/\/.*$/gm, '');
  const relWith = (needle: string): string[] =>
    sources()
      .filter((p) => stripComments(readFileSync(p, 'utf8')).includes(needle))
      .map((p) => p.slice(SRC.length))
      .sort();

  // 变异对照：把任一入口改回 `useStagedConfigStore((s) => s.enabled)` → 本条转红。
  it('读 store 原始开关的地方只剩 useStagingActive 这一处', () => {
    expect(relWith('(s) => s.enabled')).toEqual(['store/use-staging-active.ts']);
  });

  // 所有 editRoute / splitPatchByRoute 调用点都必须在拿过 useStagingActive 的文件里。
  // 变异对照：新增一个编辑入口、自己取 `STAGED_CONFIG_ENABLED` 常量喂进去 → 本条转红。
  it('每个 editRoute / splitPatchByRoute 调用点所在文件都取了 useStagingActive', () => {
    const routers = new Set([...relWith('editRoute('), ...relWith('splitPatchByRoute(')]);
    // 判定本体与按键分流器自身不是「编辑入口」，它们收 boolean 实参。
    routers.delete('lib/staged-config.ts');
    routers.delete('components/screens/settings/config-patch-route.ts');
    const holders = new Set(relWith('useStagingActive()'));
    expect([...routers].filter((f) => !holders.has(f))).toEqual([]);
  });
});
