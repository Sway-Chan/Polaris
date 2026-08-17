/**
 * CI 工具链钉扎常量的**跨 workflow 对拍门**。
 *
 * `ci.yml`（检测门）与 `package.yml`（出包腿）各自内联一份「固定版本 + 固定 URL + sha256」
 * 的 protoc / NASM 安装步骤 —— 这是 2026-08-17 把 `arduino/setup-protoc@v3` 与
 * `ilammy/setup-nasm@v1` 换掉时的形态（换掉的理由写在 `ci.yml` 那两步上方）。
 *
 * 换掉 action 消掉了两个外部失效点，但**引入了一个新的失效模式**：同一组常量现在有两份。
 * 只改一份不会有任何东西变红 —— 两条腿照样各自装出一个自洽的 protoc，各自的断言步也照样绿，
 * 于是「发布产物用的 protoc」和「门验过的 protoc」可以是两个版本，而没人知道。
 * 这道门就是为这一个缺口建的：两份常量必须逐字相同。
 *
 * # 判据
 *
 * - 两个文件里的 `PROTOC_VERSION` / `NASM_VERSION` 相同；
 * - 两个文件里的 `(平台 → sha256)` 表逐条相同，且**条数固定**——少一行也红。
 *   固定条数是刻意的：只比对「两边相等」的话，把两边的表一起删空同样相等，门就成了摆设。
 * - 两个文件都还在真的**校验**那些 sha256（`sha256sum -c` / `shasum -a 256 -c`），
 *   而不是只把常量写在那儿；
 * - 两个文件都还留着装完之后的 PATH 断言步。删掉它，安装步就退化成一个静默 no-op，
 *   构建会悄悄用 runner 上碰巧存在的另一份 protoc。
 *
 * # 这门抓不到什么（别当成「protoc 钉扎都验过了」）
 *
 * - **sha256 对不对**：它只保证两处相同。两处一起写错，本门全绿，真正会红的是 CI 上的
 *   `sha256sum -c`（那才是校验本身）。
 * - **版本选得对不对**：选 35.1 的依据（三个版本产出的 FileDescriptorSet 逐字节相同）
 *   在 `ci.yml` 的注释里，是一次性的实测结论，本门不复验。
 * - **URL 还在不在**：资产被上游删除只有真跑 CI 才知道。
 * - `ui.yml` 的 pnpm 钉扎不在本门射程内 —— 它只有一处，没有对拍对象。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));

const WORKFLOWS = ['ci.yml', 'package.yml'] as const;

/** protoc 官方 release 覆盖的平台数（`RUNNER_OS-RUNNER_ARCH` → 资产名）。 */
const PROTOC_PLATFORM_ROWS = 4;

function read(workflow: string): string {
  return readFileSync(join(REPO_ROOT, '.github/workflows', workflow), 'utf8');
}

/** 取 `NAME: '<value>'`（workflow step 的 env 钉扎）。 */
function envPin(src: string, name: string): string[] {
  return [...src.matchAll(new RegExp(`^\\s*${name}: '([^']+)'$`, 'gm'))].map((m) => m[1]);
}

/** 取 `asset='<平台资产名>'; sha256='<64 位十六进制>'`，按资产名排序后对拍。 */
function protocAssetPins(src: string): string[] {
  return [...src.matchAll(/asset='([^']+)';\s*sha256='([0-9a-f]{64})'/g)]
    .map((m) => `${m[1]}=${m[2]}`)
    .sort();
}

/** 取独立成行的 `sha256='<64 位十六进制>'`（NASM 那份）。 */
function standaloneShaPins(src: string): string[] {
  return [...src.matchAll(/^\s*sha256='([0-9a-f]{64})'$/gm)].map((m) => m[1]).sort();
}

describe('CI 工具链钉扎常量跨 workflow 对拍', () => {
  const sources = Object.fromEntries(WORKFLOWS.map((w) => [w, read(w)])) as Record<
    (typeof WORKFLOWS)[number],
    string
  >;

  it('protoc 版本两处一致且都已钉扎', () => {
    const ci = envPin(sources['ci.yml'], 'PROTOC_VERSION');
    const pkg = envPin(sources['package.yml'], 'PROTOC_VERSION');
    expect(ci, 'ci.yml 里没有 PROTOC_VERSION —— protoc 安装步被改回浮动了？').toHaveLength(1);
    expect(pkg, 'package.yml 里没有 PROTOC_VERSION —— 出包腿的 protoc 不再钉扎').toHaveLength(1);
    expect(pkg[0], 'ci.yml 与 package.yml 的 protoc 版本不同：门验的和出包用的不是同一个').toBe(
      ci[0],
    );
  });

  it('protoc 每平台的 URL 资产名与 sha256 两处逐条一致', () => {
    const ci = protocAssetPins(sources['ci.yml']);
    const pkg = protocAssetPins(sources['package.yml']);
    // 条数固定：两边一起删空也是「相等」，那样这道门就没有牙了。
    expect(ci, `ci.yml 的 protoc 平台表应有 ${PROTOC_PLATFORM_ROWS} 条`).toHaveLength(
      PROTOC_PLATFORM_ROWS,
    );
    expect(pkg, `package.yml 的 protoc 平台表应有 ${PROTOC_PLATFORM_ROWS} 条`).toHaveLength(
      PROTOC_PLATFORM_ROWS,
    );
    expect(pkg, 'ci.yml 与 package.yml 的 protoc sha256 表漂移了：只改了一处').toEqual(ci);
  });

  it('NASM 版本与 sha256 两处一致', () => {
    const ciVer = envPin(sources['ci.yml'], 'NASM_VERSION');
    const pkgVer = envPin(sources['package.yml'], 'NASM_VERSION');
    expect(ciVer).toHaveLength(1);
    expect(pkgVer).toHaveLength(1);
    expect(pkgVer[0], 'ci.yml 与 package.yml 的 NASM 版本不同').toBe(ciVer[0]);

    const ciSha = standaloneShaPins(sources['ci.yml']);
    const pkgSha = standaloneShaPins(sources['package.yml']);
    expect(ciSha, 'ci.yml 应恰有 1 条独立 sha256（NASM）').toHaveLength(1);
    expect(pkgSha, 'package.yml 应恰有 1 条独立 sha256（NASM）').toHaveLength(1);
    expect(pkgSha, 'ci.yml 与 package.yml 的 NASM sha256 漂移了').toEqual(ciSha);
  });

  it.each(WORKFLOWS)('%s 里的 sha256 是真在校验，不是摆着看', (workflow) => {
    const src = sources[workflow];
    expect(
      src.includes('sha256sum -c -') && src.includes('shasum -a 256 -c -'),
      `${workflow} 里找不到 sha256 校验命令 —— 常量还在，校验没了`,
    ).toBe(true);
  });

  it.each(WORKFLOWS)('%s 装完仍断言 PATH 上解析到的就是钉扎的那份', (workflow) => {
    const src = sources[workflow];
    // 匹配断言里**真正的比较**，不是步骤名：只 grep 步骤名的话，把 run 块掏空、名字留着照样绿。
    expect(
      src.includes('[ "$got" = "$PROTOC_EXPECT" ]'),
      `${workflow} 丢了 protoc 的 PATH 断言 —— 安装步变成静默 no-op 也没人知道`,
    ).toBe(true);
    expect(
      src.includes('PROTOC_EXPECT=libprotoc'),
      `${workflow} 的安装步不再导出 PROTOC_EXPECT —— 断言拿不到期望值`,
    ).toBe(true);
  });
});
