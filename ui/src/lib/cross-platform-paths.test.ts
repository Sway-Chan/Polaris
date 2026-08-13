/**
 * 跨平台路径规约门 —— 仓库里的**每一个受版本控制的路径**都必须在 Windows / macOS / Linux 上都能 checkout。
 *
 * # 为什么需要它（2026-08-05，真机血证）
 *
 * `ui/src/i18n/aux.ts` 与 `ui/src/i18n/locales/aux/*.json` 共 6 个路径踩了 Windows 保留设备名。
 * 后果不是「某个测试挂了」，是 **Windows 上 `git checkout` 直接退 128**：
 *
 * ```
 * error: invalid path 'ui/src/i18n/aux.ts'
 * ```
 *
 * 即任何人在 Windows 上都 clone 不出这个仓库、也就无从构建。它 2026-07-31 引入，
 * 到 08-05 才被发现 —— 因为 `ci.yml` 的三平台矩阵只在 PR / dispatch / 发布时展开，
 * push main 只跑 ubuntu，Windows 腿整整五天没跑过。
 *
 * **本门就是补那个盲区**：它跑在 `ui.yml`（push main 即触发），不需要 Windows runner 就能拦住
 * Windows-only 的路径缺陷。等到 Windows 腿跑起来才发现，代价是那一整段时间里仓库对 Windows 是坏的。
 *
 * # 判据面
 *
 * `git ls-files`（**受版本控制的**路径，不是工作树扫描 —— 后者会把 `target/` 之类的本地产物算进来）。
 * 逐条查四类 Windows 敌对形态，每类各自成条，报错时直接给出违规路径。
 *
 * # 变异验证状态（如实标注，勿当四条都验过）
 *
 * - **保留设备名**：造 `ui/src/lib/nul.txt` + `git add -f` → 该条转红且**只红这一条**，还原后
 *   `git status` 逐字节回基线。✅ 端到端验过。
 * - **大小写冲突**：`git add` 挡不住（index 大小写不敏感，两个只差大小写的文件塞不进去），
 *   改用 `git update-index --add --cacheinfo` 直接塞进 index，与已跟踪的
 *   `settings-promise-wiring.test.ts` 造冲突 → 该条转红且只红这一条。✅ 端到端验过。
 * - **非法字符** / **尾随点空格**：未做端到端变异。两者与上面两条是同一形态的字符类正则，
 *   逻辑一致；但这是推断不是实测，别读成「四条都验过」。
 */
import { describe, expect, it } from 'vitest';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('../../..', import.meta.url));

/** 受版本控制的全部路径（仓库根的相对路径，`/` 分隔）。 */
function trackedPaths(): string[] {
  return execFileSync('git', ['ls-files', '-z'], { cwd: REPO, encoding: 'utf-8', maxBuffer: 64 * 1024 * 1024 })
    .split('\0')
    .filter((p) => p !== '');
}

/** 每个路径段（目录名与文件名），带上它所属的完整路径便于报错定位。 */
function segments(): { path: string; seg: string }[] {
  return trackedPaths().flatMap((p) => p.split('/').map((seg) => ({ path: p, seg })));
}

describe('跨平台路径规约（仓库必须能在三平台 checkout）', () => {
  /// 🔴 Windows 保留设备名。**带扩展名同样非法** —— `aux.ts` 与 `aux` 一样进不去，
  /// 因为 Win32 在解析路径时先剥扩展名再比对设备名表。这正是 2026-08-05 那次的成因。
  ///
  /// 变异锁：把 `.replace(/\..*$/, '')` 删掉 → `aux.ts` 这类漏网（只剩纯 `aux` 目录被拦），
  /// 而漏的恰好是真实发生过的那一种。
  it('不得出现 Windows 保留设备名（CON/PRN/AUX/NUL/COM1-9/LPT1-9）', () => {
    const RESERVED = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/;
    const bad = segments()
      .filter(({ seg }) => RESERVED.test(seg.toLowerCase().replace(/\..*$/, '')))
      .map(({ path }) => path);
    expect([...new Set(bad)], 'Windows 上这些路径 checkout 会直接失败（error: invalid path）').toEqual([]);
  });

  /// Windows 文件名禁用字符。`/` 不在其列 —— 它是这里的路径分隔符，已被 split 掉。
  it('不得出现 Windows 非法字符 : * ? " < > |', () => {
    const bad = segments()
      .filter(({ seg }) => /[:*?"<>|]/.test(seg))
      .map(({ path }) => path);
    expect([...new Set(bad)], 'Windows 文件名不接受这些字符').toEqual([]);
  });

  /// Windows 会**静默剥掉**结尾的点与空格（`foo.` → `foo`）⇒ checkout 出来的名字与仓库里的不一致，
  /// 后续任何按名查找都落空。比直接报错更难查，故一并拦。
  it('路径段不得以点或空格结尾', () => {
    const bad = segments()
      .filter(({ seg }) => /[. ]$/.test(seg))
      .map(({ path }) => path);
    expect([...new Set(bad)], 'Windows 会静默剥掉结尾的点/空格 → 名字对不上').toEqual([]);
  });

  /// Windows / macOS 默认大小写不敏感：两个只差大小写的路径在那里会**互相覆盖**，
  /// checkout 后少一个文件且 `git status` 显示莫名其妙的删除。
  it('不得存在仅大小写不同的同名路径', () => {
    const seen = new Map<string, string>();
    const bad: string[] = [];
    for (const p of trackedPaths()) {
      const k = p.toLowerCase();
      const prev = seen.get(k);
      if (prev !== undefined) bad.push(`${prev} ⇄ ${p}`);
      else seen.set(k, p);
    }
    expect(bad, '大小写不敏感的文件系统上这些路径会互相覆盖').toEqual([]);
  });

  /// 🔴 **正向对照**：判据面不能是空的。上面四条若因 `git ls-files` 失败而拿到空列表，
  /// 会全部「通过」—— 那是最坏的失效模式（门在，但什么都没守）。
  it('判据面非空（防 git ls-files 失败导致四条门空转全绿）', () => {
    const all = trackedPaths();
    expect(all.length, 'git ls-files 没返回任何路径 —— 上面四条门此刻全是空转').toBeGreaterThan(100);
    expect(all, '锚点文件应在判据面内').toContain('ui/src/i18n/auxiliary.ts');
  });
});
