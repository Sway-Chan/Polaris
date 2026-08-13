/**
 * **依赖布局门**：测试运行器与项目 vite 的版本必须真的相容。
 *
 * # 这道门在防什么（2026-07-31 真实发生过）
 *
 * `package.json` 曾同时声明 `vite: ^5.4.0` 与 `vitest: ^4.1.10`，而 `vitest@4` 的
 * `peerDependencies.vite` 是 `^6 || ^7 || ^8` —— **这个组合不可满足**。
 *
 * 它之所以长期没被发现，是因为两个包管理器对同一份 `package.json` 给出**不同的树**：
 *
 * - **npm**（本地实际在用的）：vitest 把 vite 同时列进 `dependencies`，npm 就给它嵌一份
 *   `vitest/node_modules/vite@8`，peer 冲突被悄悄绕过 ⇒ 本地全绿。
 * - **pnpm**（仓里唯一跟踪的 `pnpm-lock.yaml`、CI 用的那个）：严格按 peer 解析，vitest 拿到
 *   项目的 `vite@5` ⇒ 启动即
 *   `ERR_PACKAGE_PATH_NOT_EXPORTED: './module-runner' is not defined by "exports"`。
 *
 * 也就是说：**CI 的前端测试 job 一直起不来，而本地一直是绿的。** 这种失效最坏的地方不是它红，
 * 是它红在没人看的地方、同时给本地一个「全绿」的假信号。
 *
 * # 判据
 *
 * 只问一件事：**装出来的 vitest，其 `peerDependencies.vite`（若声明了）必须覆盖装出来的项目 vite**。
 * 不写死版本号 —— 写死就得跟着每次升级改，改的人多半也就顺手放宽了。
 *
 * # 这门抓不到什么
 *
 * - 它跑在**当前这棵树**的 `node_modules` 上。npm 树与 pnpm 树都能跑，但各自只证各自那棵。
 *   真正等价于 CI 的判据仍是「干净 `pnpm install --frozen-lockfile` 后跑一遍」。
 * - 其它包的 peer 冲突（只盯 vitest↔vite 这一对，因为只有它会让整个测试门起不来）。
 * - 运行期行为差异（版本相容 ≠ 行为一致）。
 */

import { describe, expect, it } from 'vitest';
import { createRequire } from 'node:module';

const require_ = createRequire(import.meta.url);

/** `^6.0.0 || ^7.0.0 || ^8.0.0` → `[6, 7, 8]`。只取主版本 —— peer 范围事实上都是按主版本列的。 */
function allowedMajors(range: string): number[] {
  return [
    ...new Set(
      range
        .split('||')
        .map((r) => /(\d+)/.exec(r.trim())?.[1])
        .filter((m): m is string => m !== undefined)
        .map(Number)
    ),
  ];
}

describe('vitest 与项目 vite 的版本相容', () => {
  it('vitest 声明的 vite peer 范围必须覆盖项目实际装的 vite', () => {
    const vitestPkg = require_('vitest/package.json') as {
      version: string;
      peerDependencies?: Record<string, string>;
    };
    const vitePkg = require_('vite/package.json') as { version: string };

    const range = vitestPkg.peerDependencies?.vite;
    if (range === undefined) {
      // vitest 3 这一支不把 vite 列为 peer（自带为普通 dependency）⇒ 结构上不可能冲突。
      expect(vitestPkg.version).toMatch(/^\d/);
      return;
    }

    const majors = allowedMajors(range);
    const viteMajor = Number(/(\d+)/.exec(vitePkg.version)?.[1]);
    expect(majors.length, `vitest peer 范围解析不出主版本：${range}`).toBeGreaterThan(0);
    expect(
      majors,
      `vitest@${vitestPkg.version} 要求 vite ${range}，而项目装的是 vite@${vitePkg.version}。\n` +
        'npm 会给 vitest 嵌一份合规的 vite 把冲突绕过去（于是本地全绿），pnpm 严格按 peer 解析' +
        '⇒ CI 的前端测试 job 起不来。修法：调整 package.json 让两者真的相容，' +
        '判据是「干净 pnpm install --frozen-lockfile 后 pnpm exec vitest run 能跑」。'
    ).toContain(viteMajor);
  });
});
