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
 *
 * # 2026-08-17：判据面在合并中的工作树里是脏的（已修）
 *
 * `git ls-files` 对**未解冲突**的路径会按每个未解 stage 各输出一行，**行数 1~3 取决于冲突类型**
 * （一次性仓实测，git 2.53：content 冲突 3 行 = stage 1/2/3；add/add 2 行 = stage 2/3；
 * modify/delete 2 行 = stage 1/3）。原实现不去重 ⇒ 「仅大小写不同」那条把 `f.txt ⇄ f.txt`
 * 报成冲突，于是任何人在 merge 未收尾时跑前端全量测试都会看到一条与自己改动无关的红。
 * 修在 `parseTrackedPaths`（按原样字符串去重），并为它补了双向对照用例。
 *
 * **判据面在合并中的索引里恰好是对的**：干净合并的路径落 stage 0（= 合并结果），只有冲突路径
 * 才铺多个 stage ⇒ 去重后拿到的不是「两侧并集」，而是**即将被 commit 的那棵树**。而这道门问的
 * 正是「这个仓 checkout 得出来吗」，所以合并态该跑，不该跳过 —— 合并恰是两侧各自新增的文件
 * 第一次共处一棵树、大小写碰撞第一次可见的时刻，那时关门等于在最需要它的时刻关门。
 */
import { describe, expect, it } from 'vitest';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('../../..', import.meta.url));

/**
 * `git ls-files -z` 的输出 → 路径集合。
 *
 * 去重不是顺手的整洁，是这条门能不能在**合并中的工作树**里跑的前提：索引里有未解冲突时
 * `git ls-files` 会把同一路径按**每个未解 stage** 各列一遍，于是下面「仅大小写不同」那条会把
 * `f.txt ⇄ f.txt` 当成冲突报出来。任何人 merge 到一半跑前端全量测试都会撞上，而且会
 * **先怀疑自己的改动** —— 门给出的信号与真实缺陷完全无关。
 *
 * 两条实现约束：
 * - 按**原样字符串**去重，**不做任何归一**。`toLowerCase()` 会让大小写门永久变绿，`trim()` 会让
 *   「尾随点空格」那条永久变绿 —— 两种都是全静默的坏法，故下方对照用例把这几个维度一次钉死。
 *   这个解析器是五条门共用的单一咽喉，它被归一一次，就有一条门被无声废掉。
 * - 不用 `git ls-files --deduplicate`。**主要理由是它不可单测**：行为塞进子进程旗标后，
 *   上面那条对照用例就没有挂载点，而变异实测证明归一方向的破坏是全静默的。
 *   （实测补充：`--deduplicate` 本身**不做**大小写折叠，语义与 `new Set` 等价 ⇒ 这个取舍
 *   只在可测性上，不在正确性上。它是 git 2.31+ 旗标这点也成立，但本仓 CI 跑 ubuntu-22.04、
 *   且 node/pnpm 的版本要求本就排除了那些老宿主，所以版本门槛不是真正的理由。）
 *
 * 抽成具名纯函数而不是把 `new Set` 内联进 `trackedPaths()`，买的就是「可挂对照用例」这一件事。
 */
function parseTrackedPaths(raw: string): string[] {
  return [...new Set(raw.split('\0').filter((p) => p !== ''))];
}

/** 受版本控制的全部路径（仓库根的相对路径，`/` 分隔）。 */
function trackedPaths(): string[] {
  return parseTrackedPaths(
    execFileSync('git', ['ls-files', '-z'], { cwd: REPO, encoding: 'utf-8', maxBuffer: 64 * 1024 * 1024 }),
  );
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

  /// 判据面的**解析**本身也要有门：上面五条都建立在 `parseTrackedPaths` 之上，
  /// 它一旦去重去错方向，损坏的是门而不是被守的代码 —— 那种坏法是静默的。
  it('同一路径的多个 merge stage 只算一条（否则合并中的工作树里大小写门恒红）', () => {
    expect(parseTrackedPaths('f.txt\0f.txt\0f.txt\0g.txt\0')).toEqual(['f.txt', 'g.txt']);
  });

  /// 🔴 反方向对照：去重**不得对路径做任何归一**。每一种归一都会静默废掉上面的一条门 ——
  /// `toLowerCase()` 废掉大小写那条，`trim()` / `replace(/[. ]+$/,'')` 废掉尾随点空格那条，
  /// 而三者都不会让任何断言转红（实测：只加 `.map(p => p.trim())` 仍是全绿）。
  /// 所以这条用例要一次覆盖全部维度，不能只钉大小写。
  it('去重不得对路径做任何归一（大小写 / 尾随点 / 尾随空格）', () => {
    expect(parseTrackedPaths('a/F.txt\0a/f.txt\0a/x.\0a/x\0a/y \0a/y\0')).toEqual([
      'a/F.txt',
      'a/f.txt',
      'a/x.',
      'a/x',
      'a/y ',
      'a/y',
    ]);
  });
});
